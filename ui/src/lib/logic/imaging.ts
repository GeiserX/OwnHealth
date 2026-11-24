// The Imaging page (slice 42): what an import.dicom job did, as lines the page shows under the upload,
// and where keyboard focus goes after a decision removes a review row. Pure: tests/imaging.test.ts runs
// it under node; the pages turn each line into text with tp.

/** One line: a dictionary key and its count (plural forms with `_one`). */
export interface ImagingLine {
	key: string;
	n: number;
}

const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);

/**
 * The lines of a finished import.dicom job (ownhealth-imaging import.rs): the studies it put on the
 * record, those already there, those waiting in Import & review and those Orthanc refused, then the
 * files it could not read. A zero count is left out, except the new studies, which always show.
 * Null for a result this member does not see (the job hides it without read:records).
 */
export function imagingLines(result: unknown): ImagingLine[] | null {
	if (!result || typeof result !== 'object') return null;
	const r = result as Record<string, unknown>;
	const lines: ImagingLine[] = [{ key: 'img_r_imported', n: num(r.imported) }];
	for (const [field, key] of [
		['already', 'img_r_already'],
		['in_review', 'img_r_in_review'],
		['refused', 'img_r_refused'],
		['damaged', 'img_r_damaged']
	] as const) {
		if (num(r[field]) > 0) lines.push({ key, n: num(r[field]) });
	}
	return lines;
}

/**
 * The rows to try for focus after a decision on row `id`, in order: the row itself (it stays when the
 * decision failed), then the rows below it, then the rows above it, nearest first. `order` is the rows
 * as the page showed them before the decision.
 */
export function focusOrder(order: string[], id: string): string[] {
	const i = order.indexOf(id);
	if (i < 0) return [];
	return [id, ...order.slice(i + 1), ...order.slice(0, i).reverse()];
}
