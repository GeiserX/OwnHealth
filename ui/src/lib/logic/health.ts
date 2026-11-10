// Pure helpers for the health-data screens (drug check, wearables), so node --test can run them.

/** Whether a withheld list names this scope. The API sends 'genome' or 'read:genome'; both count. */
export function withheldBy(byScope: readonly string[] | null | undefined, scope: string): boolean {
	return (byScope ?? []).some((x) => x === scope || x === 'read:' + scope);
}

/** A phenotype that changes what CPIC says to do (not normal function or normal metabolism). */
export function isActionable(phenotype: string | null | undefined): boolean {
	if (!phenotype) return false;
	return !/^\s*normal\b/i.test(phenotype);
}

/** Average and coverage of the days on screen: days with a value, their mean, and the share of days covered. */
export function sliceStats(vals: (number | null)[]): { avg: number | null; days: number; coverage: number } {
	const nums = vals.filter((v): v is number => v != null);
	const avg = nums.length ? nums.reduce((a, b) => a + b, 0) / nums.length : null;
	return { avg, days: nums.length, coverage: vals.length ? Math.round((nums.length / vals.length) * 100) : 0 };
}
