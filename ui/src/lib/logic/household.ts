// The household overview on the home page: one row per profile the signed-in person can see, built
// from that profile's status (GET /profiles/{p}/status, the body get_health_status serves) and its
// dashboard (GET /profiles/{p}/dashboard, what the member's own home page shows). Both answer within
// the caller's grant, so a row never counts what the person may not see; these helpers only count
// and name what the grant left out.

/** The parts of the rendered status this view reads. */
export interface StatusCounts {
	data?: {
		flags?: unknown[];
		stale?: unknown[];
		data_freshness?: Record<string, unknown>;
		medications?: unknown[];
		supplements?: unknown[];
		active_conditions?: unknown[];
	};
	withheld?: { by_scope?: string[] };
}

/** The parts of the dashboard this view reads. */
export interface DashboardCounts {
	systems?: { withheld?: boolean; markers?: { stale?: boolean }[] }[];
	/** The Due list, the same items GET /profiles/{p}/due answers. */
	prevention_due?: { status: string }[];
	prevention_withheld?: string[];
}

/** One member's row: what is flagged, what is overdue, what is out of date, and what is not shared. */
export interface MemberCounts {
	flags: number;
	overdue: number;
	stale: number;
	/** The scopes this access lacks for a full row ('read:labs'), sorted; empty when nothing is left out. */
	withheld: string[];
	/** Nothing at all is recorded for the member, and nothing is left out: the zeros are not "up to date". */
	noData: boolean;
}

/** The scopes whose data feeds a column: flags (labs, genome, records), overdue (records, labs), stale (labs, wearables). */
const FEEDS = new Set(['read:labs', 'read:records', 'read:genome', 'read:wearables']);

const some = (l: unknown[] | undefined) => (l?.length ?? 0) > 0;

/**
 * The counts of one member. Stale is what the member's dashboard marks stale (lab and wearable
 * values); the status alone lists lab recency only. The status names a left-out domain as 'labs' or
 * 'records'; the dashboard names a scope. Both become scopes, once each, so the row can say a zero is
 * partial.
 */
export function memberCounts(status: StatusCounts, dash: DashboardCounts): MemberCounts {
	const scopes = new Set<string>();
	for (const d of status.withheld?.by_scope ?? []) scopes.add(d.startsWith('read:') ? d : `read:${d}`);
	for (const s of dash.prevention_withheld ?? []) scopes.add(s);
	const withheld = [...scopes].filter((s) => FEEDS.has(s)).sort();
	const data = status.data ?? {};
	// a value on the dashboard is data too: the two reads can straddle an import
	const recorded =
		Object.keys(data.data_freshness ?? {}).length > 0 ||
		some(data.medications) ||
		some(data.supplements) ||
		some(data.active_conditions) ||
		(dash.systems ?? []).some((s) => !s.withheld && some(s.markers));
	return {
		flags: data.flags?.length ?? 0,
		overdue: (dash.prevention_due ?? []).filter((i) => i.status === 'overdue').length,
		stale: (dash.systems ?? []).reduce((n, s) => n + (s.withheld ? 0 : (s.markers ?? []).filter((m) => m.stale).length), 0),
		withheld,
		noData: !recorded && withheld.length === 0
	};
}

/** The overview is for a household: a person who sees only one profile has its dashboard alone. */
export const showHousehold = (profiles: unknown[]): boolean => profiles.length > 1;
