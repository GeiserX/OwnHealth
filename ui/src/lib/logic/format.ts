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
