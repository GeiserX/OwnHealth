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

/** Whether the end date a former grant had has passed: such a grant would have ended anyway, so it
 * cannot come back as it was (a PUT with a past end date would restore nothing). */
export function lapsed(expiresAt: string | null, now: Date = new Date()): boolean {
	return expiresAt !== null && new Date(expiresAt).getTime() <= now.getTime();
}

/** The PUT /grants body that gives one former grant back as it was: same role, same categories (an
 * empty set stays empty, since null would mean the role's default), same end date. */
export function giveBackGrant(f: FormerAccess): { user_id: string; profile: string; role: string; categories: string[]; expires_at: string | null } {
	return { user_id: f.user_id, profile: f.profile, role: f.role, categories: f.categories, expires_at: f.expires_at };
}
