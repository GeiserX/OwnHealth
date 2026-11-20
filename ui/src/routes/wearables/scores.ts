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
}

export interface VendorScores {
	days: number;
	scores: VendorScore[];
}

/** The server's categories (ownhealth-devices scores::CATEGORIES), in the order the card lists them. */
export const CATEGORIES = ['readiness', 'recovery', 'sleep', 'activity', 'stress', 'strain', 'body_battery'] as const;

export interface ScoreRow {
	provider: string;
	category: string;
	latest: VendorScore;
	/** The window's values, oldest first, for the sparkline. */
	values: number[];
}

/** One row per provider and category: categories in CATEGORIES order (unknown ones last), then provider. */
export function scoreRows(scores: VendorScore[]): ScoreRow[] {
	const groups = new Map<string, VendorScore[]>();
	for (const s of scores) {
		const key = `${s.category}\u0000${s.provider}`;
		groups.set(key, [...(groups.get(key) ?? []), s]);
	}
	const rank = (c: string) => {
		const i = (CATEGORIES as readonly string[]).indexOf(c);
		return i < 0 ? CATEGORIES.length : i;
	};
	return [...groups.values()]
		.map((g) => {
			const sorted = [...g].sort((a, b) => Date.parse(a.t) - Date.parse(b.t));
			return { provider: g[0].provider, category: g[0].category, latest: sorted[sorted.length - 1], values: sorted.map((s) => s.value) };
		})
		.sort((a, b) => rank(a.category) - rank(b.category) || a.category.localeCompare(b.category) || a.provider.localeCompare(b.provider));
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
