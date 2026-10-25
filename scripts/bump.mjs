#!/usr/bin/env node
// Sets [workspace.package] version in the root Cargo.toml (the single source
// of truth both crates inherit via `version.workspace = true`) and refreshes
// Cargo.lock to match. Does not commit or tag.
//
//   pnpm bump major        1.0.0 -> 2.0.0, the only part changed by hand.
//   node scripts/bump.mjs 1.26.298+42
//                          What release.yml runs to stamp a release's version
//                          (1.26.298.42) into its checkout. Cargo only takes
//                          three numbers, so the build number goes after a +.
//
// Releases never commit their version, so Cargo.toml keeps <major>.0.0 and
// local builds report that.
import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cargoTomlPath = path.join(repoRoot, 'Cargo.toml');

// x.y.z with an optional numeric +build. No leading zeros, as semver (and so
// Cargo) requires.
const VERSION_RE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:\+(0|[1-9]\d*))?$/;

function parseVersion(version) {
	const match = VERSION_RE.exec(version);
	if (!match) return null;
	return {
		major: Number(match[1]),
		minor: Number(match[2]),
		patch: Number(match[3]),
		build: match[4] === undefined ? null : Number(match[4]),
	};
}

function fail(message) {
	console.error(`bump: ${message}`);
	process.exit(1);
}

const arg = process.argv[2];
if (!arg) fail('missing argument, expected "major" or an explicit x.y.z[+build] version');

const toml = readFileSync(cargoTomlPath, 'utf8');

// Scope the search to the [workspace.package] table so a same-named key in
// another table can't be matched by accident. Either line ending, as a
// Windows checkout may have CRLF.
const sectionMatch = /^\[workspace\.package\]\r?\n/m.exec(toml);
if (!sectionMatch) fail('could not find [workspace.package] section in Cargo.toml');
const bodyStart = sectionMatch.index + sectionMatch[0].length;
const nextHeaderMatch = /^\[/m.exec(toml.slice(bodyStart));
const bodyEnd = nextHeaderMatch ? bodyStart + nextHeaderMatch.index : toml.length;
const sectionBody = toml.slice(bodyStart, bodyEnd);

const versionLineRe = /^version\s*=\s*"([^"]*)"/m;
const versionMatches = sectionBody.match(new RegExp(versionLineRe.source, 'gm')) ?? [];
if (versionMatches.length === 0) fail('no version key found in [workspace.package]');
if (versionMatches.length > 1) fail(`expected exactly one version key in [workspace.package], found ${versionMatches.length}`);

const currentVersionStr = versionLineRe.exec(sectionBody)[1];
const currentVersion = parseVersion(currentVersionStr);
if (!currentVersion) fail(`current version "${currentVersionStr}" in Cargo.toml is not a valid x.y.z[+build] version`);

let newVersion;
if (arg === 'major') {
	newVersion = { major: currentVersion.major + 1, minor: 0, patch: 0, build: null };
} else {
	newVersion = parseVersion(arg);
	if (!newVersion) fail(`"${arg}" is not "major" or a valid x.y.z[+build] version`);
}

const newVersionStr = `${newVersion.major}.${newVersion.minor}.${newVersion.patch}`
	+ (newVersion.build === null ? '' : `+${newVersion.build}`);

const updatedBody = sectionBody.replace(versionLineRe, `version = "${newVersionStr}"`);
const updatedToml = toml.slice(0, bodyStart) + updatedBody + toml.slice(bodyEnd);
writeFileSync(cargoTomlPath, updatedToml);

const lockResult = spawnSync('cargo', ['update', '--workspace'], { cwd: repoRoot, stdio: 'inherit' });
if (lockResult.status !== 0) fail('cargo update --workspace failed, Cargo.toml was already written - check Cargo.lock manually');

console.log(`bump: ${currentVersionStr} -> ${newVersionStr}`);
