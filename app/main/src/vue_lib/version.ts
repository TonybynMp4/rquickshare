// A release is versioned 1.26.298.42 (major, year, day of the year, build).
// Cargo only takes three numbers, so the app itself reports 1.26.298+42; show
// it the way the release and its installers spell it.
export function displayVersion(version: string): string {
	return version.replace('+', '.');
}

// Only the Release workflow stamps a version. Every other build - a local
// `pnpm tauri build`, the test bundles CI makes for each PR - reports the
// committed <major>.0.0, which no release ever is. Comparing that against the
// latest release always said "Update available", whatever the code was.
export function isReleaseBuild(version: string): boolean {
	return !/^\d+\.0\.0$/.test(version);
}

// Compares two dotted versions number by number; a missing part counts as 0.
export function isNewerVersion(candidate: string, current: string): boolean {
	const a = candidate.split('.').map(Number);
	const b = current.split('.').map(Number);
	if (a.some(Number.isNaN) || b.some(Number.isNaN)) return false;
	for (let i = 0; i < Math.max(a.length, b.length); i++) {
		const diff = (a[i] ?? 0) - (b[i] ?? 0);
		if (diff !== 0) return diff > 0;
	}
	return false;
}
