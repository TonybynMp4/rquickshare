#!/usr/bin/env node
// Claude Code PostToolUse hook: rustfmt a .rs file right after Claude edits it,
// so formatting never reaches `cargo fmt --check` in CI.
//
// Node rather than a shell one-liner because it runs the same under Git Bash,
// PowerShell and Linux shells, and needs no jq. Reads the hook payload (JSON)
// from stdin. A missing rustfmt is reported and ignored; a file rustfmt can't
// parse exits 2, which hands the error back to Claude to fix.
import { spawnSync } from 'node:child_process';

let input = '';
for await (const chunk of process.stdin) input += chunk;

const payload = JSON.parse(input || '{}');
const file = payload.tool_input?.file_path ?? payload.tool_response?.filePath;
if (typeof file !== 'string' || !file.endsWith('.rs')) process.exit(0);

// Both crates are edition 2021, and core_lib/rustfmt.toml doesn't say so, so
// pass it the way `cargo fmt` would.
const result = spawnSync('rustfmt', ['--edition', '2021', file], { encoding: 'utf8' });

if (result.error) {
	console.error(`rustfmt hook: could not run rustfmt (${result.error.message}); skipped`);
	process.exit(0);
}
if (result.status !== 0) {
	process.stderr.write(result.stderr || `rustfmt exited with ${result.status}\n`);
	process.exit(2);
}
