// The CGM CSV import result (slice 28): every count the server sends becomes a line, a reason the
// screen does not know still shows, an unknown column is named, and every line has its text in each
// registry language.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { CGM_REASONS, cgmLines } from '../src/lib/logic/cgm_import.ts';
import { LANGS } from '../src/lib/i18n/langs.ts';

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
	for (const l of LANGS) {
		const table = l.table as Record<string, string>;
		for (const k of keys) assert.ok(table[k]?.trim(), `${l.code}: ${k}`);
		// the placeholders each line fills
		assert.match(table.i2_cgm_imported, /\{n\}/, l.code);
		assert.match(table.i2_cgm_r_other, /\{code\}/, l.code);
		assert.match(table.i2_cgm_unknown_cols, /\{cols\}/, l.code);
		assert.match(table.i2_cgm_tz, /\{tz\}/, l.code);
	}
});
