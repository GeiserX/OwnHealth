// The result of a FIT activity import (job import.fit, slice 29) as the lines the upload panel shows
// under the file: what came in per metric, what did not and why, and that the route was left out.
// Pure: tests/fit_import.test.ts runs it under node; FitResult.svelte turns each line into text with tp/tf.

/** The reasons the importer counts a sample as not imported, in the order the panel lists them. */
export const FIT_REASONS = ['duplicate', 'other_sport_distance', 'bad_value', 'bad_time', 'over_limit'] as const;

/** The counts of the result's `fit` details, each with its dictionary key: what this import added. */
const COUNTS = [
	['heart_rate_minutes', 'i2_fit_hr'],
	['distances', 'i2_fit_distance'],
	['energy', 'i2_fit_energy']
] as const;

/** The same counts on a FIT document (`flags.fit`): what its file holds, whatever an import added. */
const DOC_COUNTS = [
	['heart_rate_minutes', 'i2_fit_doc_hr'],
	['distances', 'i2_fit_doc_distance'],
	['energy', 'i2_fit_doc_energy']
] as const;

/** One line: a dictionary key, with its count (plural forms) and its placeholders. */
export interface FitLine {
	key: string;
	n?: number;
	vars?: Record<string, string | number>;
}

const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);

/** The lines of an import.fit job result; null for any other result (or one hidden from this member). */
export function fitLines(result: unknown, counts: typeof COUNTS | typeof DOC_COUNTS = COUNTS): FitLine[] | null {
	if (!result || typeof result !== 'object') return null;
	const r = result as Record<string, unknown>;
	const d = r.fit;
	if (!d || typeof d !== 'object') return null;
	const details = d as Record<string, unknown>;
	const lines: FitLine[] = [{ key: 'i2_fit_activities', n: num(details.activities) }];
	for (const [field, key] of counts) lines.push({ key, n: num(details[field]) });
	const reasons = r.reasons && typeof r.reasons === 'object' ? (r.reasons as Record<string, unknown>) : {};
	for (const k of FIT_REASONS) {
		const n = num(reasons[k]);
		if (n > 0) lines.push({ key: 'i2_fit_r_' + k, n });
	}
	// a reason this screen does not know yet is still shown, by its code
	for (const [code, v] of Object.entries(reasons)) {
		if ((FIT_REASONS as readonly string[]).includes(code) || num(v) <= 0) continue;
		lines.push({ key: 'i2_fit_r_other', n: num(v), vars: { code } });
	}
	if (typeof details.device === 'string' && details.device.trim()) lines.push({ key: 'i2_fit_device', vars: { device: details.device.trim() } });
	if (details.gps_left_out === true) lines.push({ key: 'i2_fit_gps' });
	return lines;
}

/** The lines of a FIT document (`flags.fit`, what the file holds): the document view shows them. */
export function fitDocLines(flags: unknown): FitLine[] | null {
	if (!flags || typeof flags !== 'object') return null;
	return fitLines({ fit: (flags as Record<string, unknown>).fit }, DOC_COUNTS);
}

/** An import.fit job that ended with its result hidden from this member (no read:wearables there). */
export function fitHidden(job: { kind?: string; state?: string; result?: unknown } | undefined): boolean {
	return !!job && job.kind === 'import.fit' && job.state === 'done' && (job.result === null || job.result === undefined);
}

/** The document a finished import.fit upload made (the job result names it; the upload answer cannot, as
 * the job creates it). Undefined for any other job, or one whose result is hidden from this member. */
export function fitDocId(job: { kind?: string; state?: string; result?: unknown } | undefined): string | undefined {
	if (!job || job.kind !== 'import.fit' || job.state !== 'done') return undefined;
	const id = (job.result as Record<string, unknown> | null | undefined)?.document_id;
	return typeof id === 'string' && id ? id : undefined;
}
