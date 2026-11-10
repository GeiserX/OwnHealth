// Display-only formatting shared by every screen. The stored values never change.

const SUP: Record<string, string> = { '0': '⁰', '1': '¹', '2': '²', '3': '³', '4': '⁴', '5': '⁵', '6': '⁶', '7': '⁷', '8': '⁸', '9': '⁹', '-': '⁻' };

/** A unit as people write it: 'x10^9/L' → '×10⁹/L', 'm2' → 'm²', '10*3/uL' → '×10³/µL'. µ stays. */
export function unitLabel(u: string | null | undefined): string {
	if (!u) return '';
	return u
		.replace(/^(?:x|×|\*)?\s*10\s*[\^*]\s*(-?\d+)/i, (_, d: string) => '×10' + [...d].map((c) => SUP[c] ?? c).join(''))
		.replace(/(?<![A-Za-z])m2(?![\w])/g, 'm²')
		.replace(/(?<![A-Za-z])m3(?![\w])/g, 'm³')
		.replace(/\/uL\b/g, '/µL');
}

const MANUAL = new Set(['manual', 'manual entry', 'entered by hand']);

/** A source label for people: the server's 'manual' code (and the older English words for it) in the
 *  reader's language, whether alone or as the first part of 'manual · 2026-09-14'. */
export function sourceLabel(label: string | null | undefined, manualWord: string): string {
	if (!label) return '';
	const [first, ...rest] = label.split(' · ');
	return MANUAL.has(first.trim().toLowerCase()) ? [manualWord, ...rest].join(' · ') : label;
}

/** A reference range from its numbers: '0,4–4', '< 3,36', '> 30', '—'. For a unit the lab did not print. */
export function refRange(low: number | null, high: number | null, fmt: (v: number) => string): string {
	if (low != null && high != null) return `${fmt(low)}–${fmt(high)}`;
	if (high != null) return `< ${fmt(high)}`;
	if (low != null) return `> ${fmt(low)}`;
	return '—';
}

/** A typed number with a point or a comma for decimals, as the server wants it ('5,6' → '5.6'), or null. */
export function decimalText(s: string): string | null {
	const v = s.trim().replace('−', '-');
	return /^-?(\d+([.,]\d+)?|[.,]\d+)$/.test(v) ? v.replace(',', '.') : null;
}

/** True when a printed value and a stored one are the same number written two ways ('1,05' and 1.05). */
export function sameNumber(printed: string | null | undefined, stored: string | number | null | undefined): boolean {
	const d = decimalText(printed ?? '');
	return d != null && stored != null && Number(d) === Number(stored);
}

/**
 * A sentence that follows a colon in running text. Spanish writes it in lowercase ('Ha fallado una
 * vez: no se pudo conectar'); an acronym such as 'TLS' or 'IMAP' keeps its capitals.
 */
export function afterColon(s: string, lang: string): string {
	if (lang !== 'es' || s.length < 2 || s[1] !== s[1].toLocaleLowerCase('es')) return s;
	return s[0].toLocaleLowerCase('es') + s.slice(1);
}
