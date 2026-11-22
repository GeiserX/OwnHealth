// The household overview on the home page: one row per profile the signed-in person can see, built
// from that profile's status (GET /profiles/{p}/status, the body get_health_status serves) and its
// Due document (GET /profiles/{p}/due). Both answer within the caller's grant, so a row never counts
// what the person may not see; these helpers only count and name what the grant left out.

/** The parts of the rendered status this view reads. */
export interface StatusCounts {
	data?: { flags?: unknown[]; stale?: unknown[] };
	withheld?: { by_scope?: string[] };
}

/** The parts of the Due document this view reads. */
export interface DueCounts {
	items?: { status: string }[];
	withheld_scopes?: string[];
}

/** One member's row: what is flagged, what is overdue, what is out of date, and what is not shared. */
export interface MemberCounts {
	flags: number;
	overdue: number;
	stale: number;
	/** The scopes this access lacks for a full row ('read:labs'), sorted; empty when nothing is left out. */
	withheld: string[];
}

/**
 * The counts of one member. The status names a left-out domain as 'labs' or 'records'; the Due
 * document names a scope. Both become scopes, once each, so the row can say a zero is partial.
 */
export function memberCounts(status: StatusCounts, due: DueCounts): MemberCounts {
	const scopes = new Set<string>();
	for (const d of status.withheld?.by_scope ?? []) scopes.add(d.startsWith('read:') ? d : `read:${d}`);
	for (const s of due.withheld_scopes ?? []) scopes.add(s);
	return {
		flags: status.data?.flags?.length ?? 0,
		overdue: (due.items ?? []).filter((i) => i.status === 'overdue').length,
		stale: status.data?.stale?.length ?? 0,
		withheld: [...scopes].sort()
	};
}

/** The overview is for a household: a person who sees only one profile has its dashboard alone. */
export const showHousehold = (profiles: unknown[]): boolean => profiles.length > 1;
