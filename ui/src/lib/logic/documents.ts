// The Documents page: every file imported for the profile, newest first.

/**
 * The year groups of the Documents page, in list order. A document goes under the year of its
 * evidence day (its collection day, else its report day), the day the Done picker and the timeline
 * show; a document with neither date goes to one `undated` group.
 */
export function documentYears<D extends { evidence_day?: string | null }>(rows: D[], undated: string): [string, D[]][] {
	const out = new Map<string, D[]>();
	for (const d of rows) {
		const y = d.evidence_day?.slice(0, 4) || undated;
		const g = out.get(y);
		if (g) g.push(d);
		else out.set(y, [d]);
	}
	return [...out];
}
