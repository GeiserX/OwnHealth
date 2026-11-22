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
	'repeated_time',
	'cut_row'
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
	lines.push(zoneLine(details.timezone));
	// the same file read before in another zone: its readings were replaced, never stored twice
	const replaced = num(details.replaced);
	if (replaced > 0) {
		const was = typeof details.replaced_timezone === 'string' && details.replaced_timezone ? details.replaced_timezone : 'UTC';
		lines.push({ key: 'i2_cgm_replaced', n: replaced, vars: { tz: was } });
	}
	return lines;
}

function zoneLine(zone: unknown): CgmLine {
	return typeof zone === 'string' && zone ? { key: 'i2_cgm_tz', vars: { tz: zone } } : { key: 'i2_cgm_tz_utc' };
}

/** What a CGM export document is, from its flags (`flags.cgm_csv`); null for any other document. */
export interface CgmDoc {
	/** The vendor's provider code: 'libreview' | 'dexcom_clarity'. */
	vendor: string;
	/** The lines a viewer who may read wearables sees: the readings the file holds, its sensors, its zone. */
	lines: CgmLine[] | null;
}

export function cgmDoc(flags: unknown): CgmDoc | null {
	if (!flags || typeof flags !== 'object') return null;
	const d = (flags as Record<string, unknown>).cgm_csv;
	if (!d || typeof d !== 'object') return null;
	const c = d as Record<string, unknown>;
	const vendor = typeof c.vendor === 'string' ? c.vendor : '';
	// without read:wearables the server sends the vendor only
	if (!('held' in c) && !('readings' in c)) return { vendor, lines: null };
	const devices = Array.isArray(c.devices) ? c.devices.filter((x): x is string => typeof x === 'string') : [];
	const lines: CgmLine[] = [{ key: 'i2_cgm_held', n: num('held' in c ? c.held : c.readings) }];
	if (devices.length) lines.push({ key: 'i2_cgm_devices', n: devices.length, vars: { names: devices.join(', ') } });
	lines.push(zoneLine(c.timezone));
	return { vendor, lines };
}

/** A wearables tile's device, as GET /wearables/summary sends it. */
export interface TileDevice {
	source_id: number;
	provider: string;
	device: string | null;
	/** The end of the device's own id (a sensor serial), or null. */
	device_id?: string | null;
}

/**
 * The device id each tile shows, by source: only where two tiles would read the same name and vendor
 * (two sensors of one kind, one export each), so they can be told apart.
 */
export function tileIds(devices: TileDevice[]): Record<number, string> {
	const label = (d: TileDevice) => `${d.provider}\u0000${d.device ?? ''}`;
	const count = new Map<string, number>();
	for (const d of devices) count.set(label(d), (count.get(label(d)) ?? 0) + 1);
	const out: Record<number, string> = {};
	for (const d of devices) if ((count.get(label(d)) ?? 0) > 1 && d.device_id) out[d.source_id] = d.device_id;
	return out;
}
