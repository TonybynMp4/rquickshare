# CI plan

A proposal for a fuller CI suite: checks, builds, version bumping, releases.
Nothing here is implemented yet — `.github/workflows/` is untouched. Written
against the tree as of `0.13.0`.

## 1. Where we are today

Four workflows exist.

### `build.yml` — push to `master`, PRs

- `build_core`: `ubuntu-latest` + `macos-latest`. `cd ./core_lib && cargo test && cargo build`.
- `build_tauri` (`needs: build_core`): `ubuntu-24.04` + `macos-latest` + `macos-13`.
  `pnpm install && pnpm build:debug`, then `./rename_build.sh`, then uploads
  `artifact-debug-rquickshare-<os>`.

### `lint.yml` — push to `master`, PRs

- `style_rust`: nightly rustfmt, `cargo fmt --all --check` over `core_lib` and `app/main/src-tauri`.
- `style_tauri`: `pnpm lint` (eslint) in `app/main`.
- `clippy_check`: nightly clippy over both crates.

### `release.yml` — tag push (`'*'`) or `workflow_dispatch`

Same 3-OS matrix, `pnpm build` (release), `rename_build.sh`,
`softprops/action-gh-release@v2` — or `upload-artifact` when dispatched manually.

### `release-please.yml` — push to `master`

`googleapis/release-please-action@v4` run twice: once to cut the tag
(`skip-github-pull-request: true`), once to open/refresh the release PR
(`skip-github-release: true`).

### Version plumbing

Root `Cargo.toml` holds `[workspace.package] version = "0.13.0"`; both crates
take it with `version.workspace = true`; `tauri.conf.json` has no `version` key
so Tauri resolves it from the crate. New: `pnpm bump <major|minor|patch|x.y.z>`
(→ `scripts/bump.mjs`) edits that one line and re-syncs `Cargo.lock`.

## 2. Gaps

Ordered by how much they'd actually bite.

### 2.1 The release-please config no longer matches the tree — *blocking*

`release-please-config.json` `extra-files` still points at the pre-workspace layout:

| Configured target | Reality |
| --- | --- |
| `core_lib/Cargo.toml` → `package.version` | key is gone; it's `version.workspace = true` |
| `app/main/src-tauri/Cargo.toml` → `package.version` | same |
| `core_lib/Cargo.lock`, `app/main/src-tauri/Cargo.lock` | tracked but stale — both still say `rqs_lib 0.11.5` |
| `snap/snapcraft.yaml` | the `snap/` directory does not exist |

The root `Cargo.toml` — the *actual* source of truth — is not in the list at all.
`.release-please-manifest.json` says `0.12.1` while the tree is at `0.13.0`
(commit `f56d491` bumped it by hand), so the next automated release would try to
cut `0.12.2`.

Until this is fixed, automated releases either no-op on the version or produce a
tag whose artifacts carry a different version in their filenames.

### 2.2 Lockfiles are inverted

`.gitignore` ignores `Cargo.lock`, yet `core_lib/Cargo.lock` and
`app/main/src-tauri/Cargo.lock` are force-added and two releases stale, while the
root `Cargo.lock` — the one the workspace build actually resolves against — is
untracked. CI therefore re-resolves dependencies from scratch on every run: builds
are not reproducible, and a bad upstream patch release can break `master` with no
commit to blame.

### 2.3 Checks that exist but never run in CI

- `pnpm test` (vitest) — never invoked. Worth wiring up anyway, but be honest
  about what it buys today: the only test file is
  `tests/unit/example.test.ts`, which asserts `1 === 1`. The value is having the
  job in place so the first real test is cheap to add.
- `pnpm ts-check` (`vue-tsc --noEmit`) — never invoked. This one has teeth
  immediately; it's already catching a real error (§2.5).
- `cargo test` runs for `core_lib` only, never for `app/main/src-tauri`. There are
  47 test functions across `qr.rs`, `utils.rs`, `hdl/ble_receiver.rs` and
  `hdl/tests.rs` — all in `core_lib`, so today's coverage is not as thin as the
  invocation suggests, but the Tauri crate is entirely untested.
- `cargo clippy` runs **without `-D warnings`**, so `clippy_check` is green no
  matter what it prints. It cannot currently fail.

### 2.4 Coverage holes

- **No Windows** in any matrix — not in `build.yml`, not in `release.yml` — even
  though `tauri.conf.json` carries a `bundle.windows` block and a `.ico`.
- `release.yml` triggers on `tags: ['*']`, so any tag at all (`wip`, `backup-2024`)
  kicks off a full release build and publishes a GitHub Release.
- `lint.yml` has a step gated on `matrix.os == 'ubuntu-20.04'`; no matrix entry
  has that value, so the step is dead code.
- No dependency audit (`cargo audit` / `cargo deny`), no `pnpm audit`.
- Artifacts are never smoke-tested — nothing confirms the built `.deb`/`.AppImage`
  actually launches.

### 2.5 Baselines are not green

This matters more than the gaps above, because it dictates the rollout order.
Turning a check on as *blocking* today would red-wall `master`:

| Check | Current output | Exit code |
| --- | --- | --- |
| `pnpm lint` (`eslint .`) | 57 errors + 1 warning | **1** |
| `pnpm ts-check` (`vue-tsc --noEmit`) | 1 error — `src/composables/ContentStatus.vue(39,35): error TS2322` | **1** |
| `cargo check -p rqs_lib --features experimental` | 14 warnings | 0 |
| `cargo clippy` | unknown-but-nonzero; never enforced (no `-D warnings`) | 0 |

**`style_tauri` in `lint.yml` is red on `master` right now.** `eslint .` exits 1,
and `pnpm lint` is exactly that command. Verified against a pristine `HEAD`
checkout of the offending file, so this predates the current working-tree changes.

The breakdown is more encouraging than the headline:

- 56 of the 57 errors are `indent` / `vue/script-indent` in
  `src/components/HomePage.vue`, all auto-fixable with `eslint --fix`.
- The 57th is `vue/no-mutating-props` at `src/composables/ContentStatus.vue:39` —
  the *same line* as the `vue-tsc` TS2322 error. One real fix likely clears both.

So Phase 0 is a genuinely small job, not a slog. But it must land before any check
is made blocking.

## 3. Proposed shape

Five workflows, each with one job of responsibility.

### 3.1 `check.yml` — fast feedback, PRs + `master`

Everything that doesn't need a bundler. Target: under 5 minutes.

```yaml
name: Check
on: [pull_request, push: {branches: [master]}]
jobs:
  fmt:        # nightly rustfmt --check, both crates   (already exists, keep)
  clippy:     # cargo clippy --all-targets -- -D warnings
  test_rust:  # cargo test --workspace --all-features
  typecheck:  # pnpm ts-check
  lint_js:    # pnpm lint
  test_js:    # pnpm test -- --run
```

Notes:
- `cargo test --workspace` replaces the `cd core_lib` form and picks up
  `app/main/src-tauri` for free. It needs the Linux GTK/webkit dev packages, so
  factor the apt block into a composite action at
  `.github/actions/linux-deps/action.yml` rather than pasting it a fourth time.
- `clippy` gets `-D warnings` only after §4 Phase 0.
- Keep `Swatinem/rust-cache@v2` on every Rust job; add
  `cache-dependency-path: app/main/pnpm-lock.yaml` to the pnpm setup.

### 3.2 `build.yml` — PRs + `master`

Keep the existing structure; change three things.

- Drop `needs: build_core` — `check.yml` covers correctness, and serialising
  costs ~4 minutes of wall clock per run for no signal.
- Add `windows-latest` to the matrix (`target_path: app/main`,
  `cache_directory: app/main/src-tauri/target`). `rename_build.sh` is bash-only
  and matches `.deb/.rpm/.AppImage/.dmg`; either add `.msi/.exe` patterns and run
  it under `shell: bash`, or skip renaming on Windows and upload raw. Prefer the
  former so release filenames stay uniform.
- Bump the artifact retention explicitly (`retention-days: 7`) — debug bundles are
  large and currently sit for the default 90.

### 3.3 `release.yml` — tags matching `v*.*.*` only

```yaml
on:
  push:
    tags: ['v[0-9]+.[0-9]+.[0-9]+']
  workflow_dispatch:
    inputs: {tag_name: {required: true, type: string}}
```

Plus:
- A `verify` job that runs first and asserts the tag matches
  `[workspace.package] version` in the root `Cargo.toml`, failing loudly on drift.
  This is the cheapest possible guard against §2.1 recurring.
- Same Windows addition as `build.yml`.
- Keep `fail_on_unmatched_files: true`; it's the right default.

### 3.4 `release-please.yml` — unchanged triggers, fixed config

The workflow file is fine. The *config* needs rewriting (see §4 Phase 1).

### 3.5 `audit.yml` — weekly cron + on lockfile change

```yaml
on:
  schedule: [{cron: '0 6 * * 1'}]
  push:
    paths: ['**/Cargo.lock', 'app/main/pnpm-lock.yaml']
```

`cargo audit` (or `cargo deny check advisories`) and `pnpm audit --prod`.
Non-blocking on PRs; opens an issue on the scheduled run.

## 4. Version bumping and releases

Two mechanisms now coexist and need a clear division of labour.

- **`pnpm bump`** — manual, local. For deliberate version moves (pre-releases,
  a jump to `1.0.0`, resetting after a bad release). Edits the root `Cargo.toml`
  and `Cargo.lock`; deliberately does not commit or tag.
- **release-please** — automatic, on `master`. Derives the next version from
  Conventional Commits, maintains `CHANGELOG.md`, opens the release PR, and cuts
  the tag when it merges. Tagging is release-please's job alone.

The flow, once fixed:

```
commit (feat:/fix:) → master
  → release-please opens/updates "chore: release X.Y.Z" PR
     (PR body = changelog; PR diff = version bumped everywhere)
  → merge the PR
  → release-please pushes tag vX.Y.Z
  → release.yml verifies tag == Cargo.toml, builds 4 OSes, publishes the Release
```

### Fixing the release-please config

`release-please-config.json` `extra-files` should become:

```json
"extra-files": [
  { "type": "toml", "path": "Cargo.toml", "jsonpath": "$.workspace.package.version" },
  { "type": "toml", "path": "Cargo.lock",
    "jsonpath": "$.package[?(@.name.value == 'rqs_lib')].version" },
  { "type": "toml", "path": "Cargo.lock",
    "jsonpath": "$.package[?(@.name.value == 'rquickshare')].version" }
]
```

— dropping the two per-crate `Cargo.toml` entries (no `package.version` key
exists any more) and the `snap/snapcraft.yaml` entry (no such file). The two
`Cargo.lock` entries only make sense once the root lockfile is tracked (§4
Phase 1); until then, drop them too and let `release.yml`'s verify job catch drift.

`.release-please-manifest.json` must be set to `{".": "0.13.0"}` in the same
commit, or the next release regresses to `0.12.x`.

`"prerelease": true` is currently set. That's correct for `0.x`; revisit at `1.0.0`.

### Updater

`tauri.conf.json` has no `updater` plugin configured and no
`createUpdaterArtifacts`, so there is no in-app update path — users reinstall from
the GitHub Release. Adding one is a larger piece of work (signing keypair,
`TAURI_SIGNING_PRIVATE_KEY` secret, a hosted `latest.json`) and is out of scope
here; noting it as the natural follow-on once releases are trustworthy.

## 5. Rollout

### Phase 0 — clear the baselines (prerequisite, no CI changes)

Nothing below can be enforced until these are green — and one of them is red
*today*. Separate commits:

1. `chore: fix eslint indentation in HomePage.vue` — `pnpm lint --fix`, review
   the diff, commit. Clears 56 of 57.
2. `fix:` the `vm` prop mutation at `ContentStatus.vue:39`. Clears the last eslint
   error *and* the `vue-tsc` TS2322 on the same line.
3. `chore:` the clippy output for both crates.
4. `chore:` the 14 `rqs_lib --features experimental` warnings.

(1) and (2) also un-break `lint.yml` as it stands today, so they're worth doing
regardless of whether the rest of this plan happens. If (3)–(4) turn out to be
large, an acceptable interim is to enforce on changed files only and keep a
shrinking allowlist — but that's a fallback, not the plan.

### Phase 1 — make releases correct

Highest value, smallest diff, independent of Phase 0:

1. Rewrite `release-please-config.json` `extra-files`; set the manifest to `0.13.0`.
2. Track the root `Cargo.lock`; untrack the two stale per-crate ones
   (`git rm --cached`) and narrow `.gitignore` from `Cargo.lock` to
   `core_lib/Cargo.lock` / `app/main/src-tauri/Cargo.lock` — or just delete them.
3. Narrow `release.yml`'s tag filter and add the tag/version verify job.
4. Delete the dead `ubuntu-20.04` step in `lint.yml`.

Verifiable end-to-end with one throwaway `feat:` commit on a branch.

### Phase 2 — split and widen the checks

1. Add the `linux-deps` composite action.
2. Split `lint.yml` into `check.yml` with the six jobs from §3.1, all blocking.
3. Drop `needs: build_core` from `build.yml`.

### Phase 3 — Windows

1. Add `windows-latest` to `build.yml`; get a debug bundle out.
2. Teach `rename_build.sh` about `.msi`/`.exe`.
3. Add it to `release.yml`.

### Phase 4 — hardening

1. `audit.yml`.
2. Smoke-test the Linux artifact (`xvfb-run` the AppImage, assert it stays up a
   few seconds and exits cleanly).
3. Revisit the updater.

## 6. Open questions

- **Windows support**: is it actually wanted? The `bundle.windows` config and
  `.ico` suggest yes, but nothing has ever built there, so Phase 3 may surface real
  porting work in `core_lib` (mDNS, BLE, D-Bus) rather than just CI work. Worth a
  spike before committing to it in `release.yml`.
- **Snap**: `release-please-config.json` references `snap/snapcraft.yaml` and
  `.gitignore` has `*.snap`. Was snap packaging dropped, or is it meant to come
  back? The plan above assumes dropped.
- **macOS signing**: `signingIdentity: "-"` is ad-hoc signing. Released `.dmg`s
  will hit Gatekeeper. Out of scope, but it's the other half of "trustworthy
  releases".
