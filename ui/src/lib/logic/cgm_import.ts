// The result of a LibreView or Dexcom Clarity CSV import (job import.cgm_csv, slice 28) as the lines
// the upload panel shows under the file: what came in, what did not and why, and what was not read.
// Pure: tests/cgm_import.test.ts runs it under node; the panel turns each line into text with tp/tf.

/** The reasons the importer counts a reading as not imported, in the order the panel lists them. */
export const CGM_REASONS = [
	'duplicate',
	'below_sensor_range',
	'above_sensor_range',
	'bad_value',
	'bad_time',
	'ambiguous_time',
	'nonexistent_time',
	'repeated_time'
] as const;

/** One line: a dictionary key, with its count (plural forms) and its placeholders. */
export interface CgmLine {
	key: string;
	n?: number;
	vars?: Record<string, string | number>;
}

const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);

/** The lines of an import.cgm_csv job result; null for any other result. */
export function cgmLines(result: unknown): CgmLine[] | null {
	if (!result || typeof result !== 'object') return null;
	const r = result as Record<string, unknown>;
	const d = r.cgm_csv;
	if (!d || typeof d !== 'object') return null;
	const details = d as Record<string, unknown>;
	const reasons = r.reasons && typeof r.reasons === 'object' ? (r.reasons as Record<string, unknown>) : {};
	const lines: CgmLine[] = [{ key: 'i2_cgm_imported', n: num(r.accepted) }];
	for (const k of CGM_REASONS) {
		const n = num(reasons[k]);
		if (n > 0) lines.push({ key: 'i2_cgm_r_' + k, n });
	}
	// a reason this screen does not know yet is still shown, by its code
	for (const [code, v] of Object.entries(reasons)) {
		if ((CGM_REASONS as readonly string[]).includes(code) || num(v) <= 0) continue;
		lines.push({ key: 'i2_cgm_r_other', n: num(v), vars: { code } });
	}
	const other = num(details.other_rows);
	if (other > 0) lines.push({ key: 'i2_cgm_other_rows', n: other });
	const unknown = Array.isArray(details.unknown_columns) ? details.unknown_columns.filter((c): c is string => typeof c === 'string') : [];
	if (unknown.length) lines.push({ key: 'i2_cgm_unknown_cols', n: unknown.length, vars: { cols: unknown.join(', ') } });
	const tz = typeof details.timezone === 'string' && details.timezone ? details.timezone : null;
	lines.push(tz ? { key: 'i2_cgm_tz', vars: { tz } } : { key: 'i2_cgm_tz_utc' });
	return lines;
}
