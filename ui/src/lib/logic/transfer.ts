// Ownership transfer at majority. When a child past the age of majority claims her profile, the
// server ends each guardian's grant and writes one audit row per person
// (`grant_change`, args `majority;role=guardian;categories=a,b`). These helpers read them back.

type AuditRow = {
	id: number;
	ts: string;
	action: string;
	profile: string | null;
	resource_ids?: string[];
	args_summary?: string | null;
};

export type FormerAccess = { user_id: string; role: string; categories: string[]; ts: string };

/** The people whose access to `slug` ended at the claim and who have no grant on it now, latest row first. */
export function formerAccess(rows: AuditRow[], grants: { user_id: string; profile: string }[], slug: string): FormerAccess[] {
	const out: FormerAccess[] = [];
	for (const r of [...rows].sort((a, b) => b.id - a.id)) {
		const user = r.resource_ids?.[0];
		const args = r.args_summary ?? '';
		if (r.action !== 'grant_change' || r.profile !== slug || !user || !args.startsWith('majority;')) continue;
		if (out.some((f) => f.user_id === user) || grants.some((g) => g.user_id === user && g.profile === slug)) continue;
		const field = (k: string) => args.split(';').find((p) => p.startsWith(k + '='))?.slice(k.length + 1) ?? '';
		out.push({ user_id: user, role: field('role'), categories: field('categories').split(',').filter(Boolean), ts: r.ts });
	}
	return out;
}
