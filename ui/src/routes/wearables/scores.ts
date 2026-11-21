// Vendor scores (migration 1220, GET /profiles/{p}/vendor-scores): readiness, Body Battery and the like
// are the vendor's own numbers. The Wearables page shows them in their own card, one row per provider
// and category, and never next to the measurements.

export interface VendorScore {
	provider: string;
	category: string;
	t: string;
	value: number;
	qualifier: string | null;
	components: Record<string, unknown> | null;
	computed_by: 'provider';
	/** The connection that brought the score; null once that connection is removed or moved to another person. */
	source: { id: string; label: string } | null;
}

export interface VendorScores {
	days: number;
	scores: VendorScore[];
}

/** The longest window the route serves (`days` 1 to 365). The card reads it all once and shows the
 * page's window of it, so the page can offer a window as long as the scores reach. */
export const MAX_DAYS = 365;

const DAY_MS = 86_400_000;

/** How many days back the oldest score reaches from `now`, rounded up; 0 with no score. */
export function scoreSpanDays(scores: VendorScore[], now: number): number {
	let oldest = Infinity;
	for (const s of scores) oldest = Math.min(oldest, Date.parse(s.t));
	return Number.isFinite(oldest) ? Math.max(0, Math.ceil((now - oldest) / DAY_MS)) : 0;
}

/** The scores of the last `days` before `now`, as the route sends them for that window. */
export function inWindow(scores: VendorScore[], days: number, now: number): VendorScore[] {
	const since = now - days * DAY_MS;
	return scores.filter((s) => Date.parse(s.t) >= since);
}

/** The windows the Wearables page offers: 7 days always, 30 days and a year once the longest span, of a
 * measurement series or of the vendor scores, fills them. */
export function windowChoices(span: number): number[] {
	return [7, 30, 365].filter((d) => d === 7 || span >= d);
}

/** The page's window before any choice. */
export const DEFAULT_DAYS = 30;

/** The window the page shows once both reads of a profile are in (`ready`): the chosen one while the page
 * offers it, else the longest it offers, pressed. With no choice to offer (7 days only) it is
 * DEFAULT_DAYS, which the charts cut to the data. While a read runs the list is short and not final, so
 * the choice stays: a switch between two profiles with a year of scores keeps the year. */
export function keepWindow(days: number, windows: number[], ready: boolean): number {
	if (!ready) return days;
	if (windows.length < 2) return DEFAULT_DAYS;
	return windows.includes(days) ? days : windows[windows.length - 1];
}

/** The dictionary key that names a window of `d` days, and the number for its `{n}`: '7 d', '30 d' and
 * '1 y' for the windows the page offers, 'n d' for any other, such as the 90 days of measurements shown
 * under a year picked for the scores. */
export function windowWordKey(d: number): ['h1_w7' | 'h1_w30' | 'h1_w1y' | 'h1_wn', number] {
	return [d === 7 ? 'h1_w7' : d === 30 ? 'h1_w30' : d === 365 ? 'h1_w1y' : 'h1_wn', d];
}

/** The days the measurement charts show and label for the chosen window: the window, cut to the longest
 * measurement series. The route takes 7 to 366 days, and the page asks it for its default of 90. The
 * scores card keeps the whole window, so a year of scores never labels 90 days of measurements as a year. */
export function measurementDays(days: number, span: number): number {
	return span > 0 ? Math.min(days, span) : days;
}

/** The server's categories (ownhealth-devices scores::CATEGORIES), in the order the card lists them. */
export const CATEGORIES = ['readiness', 'recovery', 'sleep', 'activity', 'stress', 'strain', 'body_battery'] as const;

export interface ScoreRow {
	provider: string;
	category: string;
	source: VendorScore['source'];
	latest: VendorScore;
	/** The window's values, oldest first, for the sparkline. */
	values: number[];
}

/** One row per provider, category and connection: categories in CATEGORIES order (unknown ones last),
 * then provider, then connection. Two connections that bring the same vendor's score keep a row each. */
export function scoreRows(scores: VendorScore[]): ScoreRow[] {
	// Linear in the number of scores: each one is pushed onto its group once, with its time parsed once.
	const groups = new Map<string, { ms: number; s: VendorScore }[]>();
	for (const s of scores) {
		const key = `${s.category}\u0000${s.provider}\u0000${s.source?.id ?? ''}`;
		let g = groups.get(key);
		if (!g) groups.set(key, (g = []));
		g.push({ ms: Date.parse(s.t), s });
	}
	const rank = (c: string) => {
		const i = (CATEGORIES as readonly string[]).indexOf(c);
		return i < 0 ? CATEGORIES.length : i;
	};
	return [...groups.values()]
		.map((g) => {
			const first = g[0].s;
			g.sort((a, b) => a.ms - b.ms);
			return { provider: first.provider, category: first.category, source: first.source, latest: g[g.length - 1].s, values: g.map((x) => x.s.value) };
		})
		.sort(
			(a, b) =>
				rank(a.category) - rank(b.category) ||
				a.category.localeCompare(b.category) ||
				a.provider.localeCompare(b.provider) ||
				(a.source?.label ?? '\uffff').localeCompare(b.source?.label ?? '\uffff')
		);
}

// Brand names are never translated.
const BRANDS: Record<string, string> = {
	oura: 'Oura',
	garmin: 'Garmin',
	whoop: 'WHOOP',
	polar: 'Polar',
	withings: 'Withings',
	fitbit: 'Fitbit',
	samsung: 'Samsung',
	coros: 'COROS',
	suunto: 'Suunto'
};

/** The vendor's name as it writes it; an unknown key ('ultrahuman') gets a capital letter. */
export function providerLabel(p: string): string {
	return BRANDS[p] ?? p.split('_').map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join(' ');
}

/** Scores are shown as the vendor sends them: whole numbers as whole, others with one decimal. */
export function scoreDecimals(v: number): number {
	return Number.isInteger(v) ? 0 : 1;
}

/** A score's day, as the card prints it next to the vendor: in UTC, like the measurement charts of the
 * same page, so a score keyed at midnight UTC keeps its day in a browser west of UTC. */
export function fmtScoreDay(iso: string, lang: string): string {
	return new Intl.DateTimeFormat(lang === 'es' ? 'es-ES' : 'en-GB', { day: 'numeric', month: 'short', timeZone: 'UTC' }).format(new Date(iso));
}
