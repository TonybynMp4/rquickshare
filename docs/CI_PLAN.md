# CI plan

A plan for a fuller CI suite: checks, builds, version bumping, releases.
Written against the tree as of `0.13.0`. **Phases 0–2 and 4.1 have landed**;
§1 and §2 below describe the *starting* state and are kept as the record of why
each change was made. See §5 for what is done and what is left.

Windows is out of scope by decision — see §6.

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
- Bump the artifact retention explicitly (`retention-days: 7`) — debug bundles are
  large and currently sit for the default 90.
- Pass `debug` as `rename_build.sh`'s third argument, so debug and release bundles
  stop sharing a filename.
- Hoist the per-entry `target_path` / `name` / `dependencies` / `cache_directory`
  matrix columns into job-level `env` and the `linux-deps` composite action; the
  matrix is then just the OS list.

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
- Keep `fail_on_unmatched_files: true`; it's the right default.
- `workflow_dispatch`'s `tag_name` input is now actually used: it drives the
  checkout `ref`, so a manual run rebuilds the tag it names instead of `master`.

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
  → release.yml verifies tag == Cargo.toml, builds 3 OSes, publishes the Release
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

### Phase 0 — clear the baselines (prerequisite, no CI changes) — **done**

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

### Phase 1 — make releases correct — **done**

Highest value, smallest diff, independent of Phase 0:

1. Rewrite `release-please-config.json` `extra-files`; set the manifest to `0.13.0`.
2. Track the root `Cargo.lock`; untrack the two stale per-crate ones
   (`git rm --cached`) and narrow `.gitignore` from `Cargo.lock` to
   `core_lib/Cargo.lock` / `app/main/src-tauri/Cargo.lock` — or just delete them.
3. Narrow `release.yml`'s tag filter and add the tag/version verify job.
4. Delete the dead `ubuntu-20.04` step in `lint.yml` — moot, the file is gone.

Verifiable end-to-end with one throwaway `feat:` commit on a branch.

### Phase 2 — split and widen the checks — **done**

1. Added the `linux-deps` composite action.
2. Replaced `lint.yml` with `check.yml` (`fmt`, `clippy`, `test_rust`, and a
   `frontend` matrix of `lint` / `ts-check` / `test`), all blocking.
3. Dropped `needs: build_core` from `build.yml` — and `build_core` itself, since
   `check.yml`'s `cargo test --workspace` strictly supersedes it.

Two things worth recording from the implementation:

- `pnpm/action-setup` reads `packageManager` from `app/main/package.json` via an
  explicit `package_json_file`; there is no `package.json` at the repo root.
- The frontend `test` job runs with `--passWithNoTests`, so deleting the
  `1 === 1` stub in `tests/unit/example.test.ts` doesn't turn the job red for the
  wrong reason. As §2.3 says, the value is having it wired up before the first
  real test, not the coverage it provides today.

### Phase 3 — hardening

1. `audit.yml` — **done**. `rustsec/audit-check` plus `pnpm audit --prod`, weekly
   and on lockfile change. Advisories only fail the workflow on the scheduled run;
   on a PR they annotate, because a CVE published overnight is not the PR's fault.
2. Smoke-test the Linux artifact (`xvfb-run` the AppImage, assert it stays up a
   few seconds and exits cleanly). *Not done.*
3. Revisit the updater. *Not done.*

## 6. Open questions

- ~~**Windows support**: is it actually wanted?~~ **Answered: no.** Not being
  pursued, so no `windows-latest` entry in either matrix and `rename_build.sh`
  stays `.deb`/`.rpm`/`.AppImage`/`.dmg` only. The `bundle.windows` config and
  `.ico` in `tauri.conf.json` are left alone; reviving this means a `core_lib`
  porting spike (mDNS, BLE, D-Bus), not just CI work.
- **Snap**: `release-please-config.json` references `snap/snapcraft.yaml` and
  `.gitignore` has `*.snap`. Was snap packaging dropped, or is it meant to come
  back? The plan above assumes dropped.
- **macOS signing**: `signingIdentity: "-"` is ad-hoc signing. Released `.dmg`s
  will hit Gatekeeper. Out of scope, but it's the other half of "trustworthy
  releases".
