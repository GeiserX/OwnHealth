// The CGM CSV import result (slice 28): every count the server sends becomes a line, a reason the
// screen does not know still shows, an unknown column is named, and every line has its text in each
// registry language.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { CGM_REASONS, cgmDoc, cgmLines, tileIds } from '../src/lib/logic/cgm_import.ts';
import { errKey } from '../src/lib/logic/errors.ts';
import { LANGS } from '../src/lib/i18n/langs.ts';

const CGM_ERRORS = ['err_cgm_unreadable', 'err_cgm_too_many', 'err_cgm_too_large', 'err_cgm_too_many_sensors'];

const clarity = {
	document_id: 'd1',
	accepted: 8,
	queued: 0,
	skipped: 2,
	reasons: { below_sensor_range: 1, above_sensor_range: 1 },
	cgm_csv: { vendor: 'dexcom_clarity', unit: 'mg/dL', readings: 8, duplicates: 0, devices: ['Dexcom G7'], unknown_columns: [], other_rows: 2, timezone: null, again: false }
};

test('a Clarity result lists what came in, Low and High, the rows left out and the time rule', () => {
	assert.deepEqual(cgmLines(clarity), [
		{ key: 'i2_cgm_imported', n: 8 },
		{ key: 'i2_cgm_r_below_sensor_range', n: 1 },
		{ key: 'i2_cgm_r_above_sensor_range', n: 1 },
		{ key: 'i2_cgm_other_rows', n: 2 },
		{ key: 'i2_cgm_tz_utc' }
	]);
});

test('the same file again, an unknown column, an unknown reason and a time zone', () => {
	const again = {
		...clarity,
		accepted: 0,
		reasons: { duplicate: 8, ambiguous_time: 24, some_new_reason: 3, zero: 0 },
		cgm_csv: { ...clarity.cgm_csv, other_rows: 0, unknown_columns: ['Glucosa del sensor adicional mg/dL', 'X'], timezone: 'Europe/Madrid' }
	};
	assert.deepEqual(cgmLines(again), [
		{ key: 'i2_cgm_imported', n: 0 },
		{ key: 'i2_cgm_r_duplicate', n: 8 },
		{ key: 'i2_cgm_r_ambiguous_time', n: 24 },
		{ key: 'i2_cgm_r_other', n: 3, vars: { code: 'some_new_reason' } },
		{ key: 'i2_cgm_unknown_cols', n: 2, vars: { cols: 'Glucosa del sensor adicional mg/dL, X' } },
		{ key: 'i2_cgm_tz', vars: { tz: 'Europe/Madrid' } }
	]);
});

test('another job result is not a CGM result', () => {
	for (const r of [null, undefined, 3, 'x', { accepted: 3, reasons: {} }, { cgm_csv: null }]) assert.equal(cgmLines(r), null);
});

test('every CGM line has its text, singular and plural, in every registry language', () => {
	const keys = [
		'i2_cgm_imported',
		...CGM_REASONS.map((r) => 'i2_cgm_r_' + r),
		'i2_cgm_r_other',
		'i2_cgm_other_rows',
		'i2_cgm_unknown_cols'
	].flatMap((k) => [k, k + '_one']);
	keys.push('i2_cgm_tz', 'i2_cgm_tz_utc', 'i2_ik_libreview_csv', 'i2_ik_dexcom_clarity_csv', 'fu_k_libreview', 'fu_k_dexcom_clarity');
	keys.push('i2_cgm_replaced', 'i2_cgm_replaced_one', 'i2_cgm_held', 'i2_cgm_held_one', 'i2_cgm_devices', 'i2_cgm_devices_one');
	keys.push('i2_doc_cgm', 'i2_doc_cgm_rows', 'i2_dt_cgm', ...CGM_ERRORS);
	for (const l of LANGS) {
		const table = l.table as Record<string, string>;
		for (const k of keys) assert.ok(table[k]?.trim(), `${l.code}: ${k}`);
		// the placeholders each line fills
		assert.match(table.i2_cgm_imported, /\{n\}/, l.code);
		assert.match(table.i2_cgm_r_other, /\{code\}/, l.code);
		assert.match(table.i2_cgm_unknown_cols, /\{cols\}/, l.code);
		assert.match(table.i2_cgm_tz, /\{tz\}/, l.code);
		assert.match(table.i2_cgm_replaced, /\{n\}.*\{tz\}|\{tz\}.*\{n\}/, l.code);
		assert.match(table.i2_doc_cgm, /\{vendor\}/, l.code);
		// the unsupported-file text names the CGM exports this importer reads
		assert.match(table.err_unsupported_file, /LibreView/, l.code);
		assert.match(table.err_unsupported_file, /Dexcom Clarity/, l.code);
	}
});

test('a file cut short, and the same file read again in another zone', () => {
	const cut = { ...clarity, accepted: 7, reasons: { cut_row: 1 } };
	assert.deepEqual(cgmLines(cut)?.slice(0, 2), [
		{ key: 'i2_cgm_imported', n: 7 },
		{ key: 'i2_cgm_r_cut_row', n: 1 }
	]);
	const moved = { ...clarity, cgm_csv: { ...clarity.cgm_csv, timezone: 'Europe/Madrid', replaced: 8, replaced_timezone: null } };
	assert.deepEqual(cgmLines(moved)?.slice(-2), [
		{ key: 'i2_cgm_tz', vars: { tz: 'Europe/Madrid' } },
		{ key: 'i2_cgm_replaced', n: 8, vars: { tz: 'UTC' } }
	]);
	// nothing replaced: no line
	assert.ok(!cgmLines({ ...clarity, cgm_csv: { ...clarity.cgm_csv, replaced: 0 } })?.some((l) => l.key === 'i2_cgm_replaced'));
});

test('a CGM document says what it is; its counts only when the server sent them', () => {
	const flags = { import: { accepted: 8 }, cgm_csv: { ...clarity.cgm_csv, held: 8, timezone: 'Europe/Madrid' } };
	assert.deepEqual(cgmDoc(flags), {
		vendor: 'dexcom_clarity',
		lines: [
			{ key: 'i2_cgm_held', n: 8 },
			{ key: 'i2_cgm_devices', n: 1, vars: { names: 'Dexcom G7' } },
			{ key: 'i2_cgm_tz', vars: { tz: 'Europe/Madrid' } }
		]
	});
	// a viewer without read:wearables gets the vendor only
	assert.deepEqual(cgmDoc({ cgm_csv: { vendor: 'libreview' } }), { vendor: 'libreview', lines: null });
	for (const f of [null, {}, { cgm_csv: null }, { apple_health: {} }]) assert.equal(cgmDoc(f), null);
});

test('two sensor tiles of one name and vendor show the end of their serial, a lone tile does not', () => {
	const g7 = (id: number, device_id: string | null) => ({ source_id: id, provider: 'dexcom_clarity', device: 'Dexcom G7', device_id });
	assert.deepEqual(tileIds([g7(1, 'iOS G7'), g7(2, 'Android G7'), { source_id: 3, provider: 'libreview', device: 'FreeStyle Libre 3', device_id: '…1A9C3B' }]), {
		1: 'iOS G7',
		2: 'Android G7'
	});
	assert.deepEqual(tileIds([g7(1, 'iOS G7'), { ...g7(2, null), provider: 'libreview' }]), {});
});

test('a refused CGM export reads in the person\'s language, not in the server\'s English', () => {
	const has = () => true;
	const cases: [string, string][] = [
		['not a readable LibreView or Dexcom Clarity export: no glucose column in mg/dL or mmol/L', 'err_cgm_unreadable'],
		['the export holds more than 5000000 readings: export a shorter period', 'err_cgm_too_many'],
		['the export is larger than 256 MB: export a shorter period', 'err_cgm_too_large'],
		["the export names more than 32 sensors: export one person's sensors, or a shorter period", 'err_cgm_too_many_sensors']
	];
	for (const [m, k] of cases) assert.equal(errKey(null, m, has), k, m);
});
