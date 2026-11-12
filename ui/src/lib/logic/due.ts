// The Due screen and the dashboard's Due card: screenings, vaccines and lab gaps from the prevention
// document (GET /profiles/{p}/due, and the dashboard's `prevention_due`). The server computes every
// status and date; these helpers only merge, count and pick a colour.

export type DueStatus = 'overdue' | 'due' | 'upcoming' | 'up_to_date' | 'not_assessed';

/** One item of ownhealth.prevention/v1 as the server renders it in one language. */
export interface DueItem {
	key: string;
	rule: string;
	area: 'screening' | 'vaccine' | 'lab';
	kind: string;
	status: DueStatus;
	due?: string;
	last?: { date: string; src?: string | null };
	source?: string;
	cite?: string | null;
	src: string[];
	group?: string;
	labs?: boolean;
	title: string | null;
	why: string | null;
	reason?: string | null;
	reason_code?: string;
	/** A guideline row: the record kind and the name that make a new record count for it. */
	record?: { kind: 'immunization' | 'procedure'; text: string | null };
}

export interface DueDoc {
	schema: string;
	profile: string;
	as_of: string;
	rules?: string;
	country?: string;
	reason_code?: string;
	reason?: string | null;
	disclaimer?: string | null;
	items: DueItem[];
	unmatched?: { name: string; src: string }[];
	withheld: number;
	/** The scopes the withheld items need. */
	withheld_scopes?: string[];
	prevention_hash: string;
	lang: string;
}

/**
 * The scopes an empty Due card lacks, as one string ("read:labs, read:records"), or null when the
 * card has rows or nothing is withheld: an empty card that withholds items must not read as nothing due.
 */
export function lockedScopes(rows: CardRow[], withheld: string[] | undefined): string | null {
	return !rows.length && withheld?.length ? withheld.join(', ') : null;
}

/** The scopes a Due card with rows still leaves items out for, or null: a partial card must say so. */
export function partialScopes(rows: CardRow[], withheld: string[] | undefined): string | null {
	return rows.length && withheld?.length ? withheld.join(', ') : null;
}

/** The Due screen lists prevention items only, which need read:records: without it the card links nowhere. */
export function dueScreenOpen(withheld: string[] | undefined): boolean {
	return !withheld?.includes('read:records');
}

/** A row of the dashboard's older `due` list (lab gaps of the rule pack, grouped by rule). */
export interface GlanceDue {
	title: string;
	why: string;
	status: string;
	kind: string;
	phase: string | null;
	rule?: string;
}

const OPEN: DueStatus[] = ['overdue', 'due', 'upcoming'];
export const isOpen = (i: { status: string }) => (OPEN as string[]).includes(i.status);

/** The status colour key (colors.ts C) of a due status. */
export function dueColor(s: string): 'out' | 'watch' | 'ok' | 'nodata' | 'stale' {
	switch (s) {
		case 'overdue':
			return 'out';
		case 'due':
			return 'watch';
		case 'upcoming':
			return 'stale';
		case 'up_to_date':
			return 'ok';
		default:
			return 'nodata';
	}
}

/** A row of the Due card; `color` is a key of colors.ts C and `word` the dict key of its status. */
export type CardRow = { key: string; title: string; why: string; color: string; word: string; area: string; phase: string | null };

/**
 * The Due card: the open prevention items, then each older `due` row whose rule no prevention item
 * has (a token without read:records gets no prevention items, so it keeps the older list). Nothing
 * shows twice: a lab gap the prevention list holds is shown from there only.
 */
export function dueCard(glance: GlanceDue[], prevention: DueItem[] | undefined): CardRow[] {
	const items = prevention ?? [];
	const rules = new Set(items.map((i) => i.rule));
	const rows: CardRow[] = items.filter(isOpen).map((i) => ({
		key: i.key,
		title: i.title ?? i.key,
		why: i.why ?? '',
		color: dueColor(i.status),
		word: `due_st_${i.status}`,
		area: i.area,
		phase: null
	}));
	for (const g of glance) {
		if (g.rule && rules.has(g.rule)) continue;
		rows.push({ key: `glance:${g.rule ?? g.title}`, title: g.title, why: g.why, color: g.status, word: g.status, area: g.kind, phase: g.phase });
	}
	return rows;
}
