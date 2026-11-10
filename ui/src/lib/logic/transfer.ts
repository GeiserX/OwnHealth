// Ownership transfer at majority. When a child past the age of majority claims her profile, the
// server ends each guardian's grant, and GET /household lists them for her as `former_access`.
// These helpers turn one entry into what her "give access back" row says.

/** One person whose access to the caller's profile ended when she took it over (GET /household). */
export type FormerAccess = {
	profile: string;
	user_id: string;
	role: string;
	categories: string[];
	expires_at: string | null;
	ended_at: string;
};

/** The dictionary keys of what a grant with these categories gives back, and whether it reaches the
 * sensitive facts or the genome. */
export function backSummary(categories: string[]): { keys: string[]; sensitive: boolean } {
	return {
		keys: categories.map((c) => 'fu_sc_' + c.replace(':', '_')),
		sensitive: categories.some((c) => c === 'read:sensitive' || c.startsWith('read:genome'))
	};
}

/** The local calendar day of an instant, as YYYY-MM-DD: a claim just after local midnight reads as
 * that day, not the UTC one before it. */
export function localDay(iso: string): string {
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return '—';
	const pad = (n: number) => String(n).padStart(2, '0');
	return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}
