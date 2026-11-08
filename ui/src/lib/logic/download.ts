/** File name from a Content-Disposition header (filename* first, then filename), else the fallback. */
export function filenameFromDisposition(header: string | null, fallback: string): string {
	if (!header) return fallback;
	const star = /filename\*\s*=\s*(?:UTF-8|utf-8)''([^;]+)/.exec(header);
	if (star) {
		try {
			return decodeURIComponent(star[1].trim().replace(/^"|"$/g, ''));
		} catch {
			/* fall through */
		}
	}
	const plain = /filename\s*=\s*("([^"]*)"|[^;]+)/.exec(header);
	const name = plain ? (plain[2] ?? plain[1]).trim() : '';
	return name || fallback;
}
