// What a FIT activity import shows under the uploaded file (slice 29): counts per metric from the job
// result, the reasons the rest was left out, the device, and that the route was not stored; a refused
// file reads as our sentence in each registry language, never as the server's English.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FIT_REASONS, fitDocId, fitDocLines, fitHidden, fitLines } from '../src/lib/logic/fit_import.ts';
import { errKey } from '../src/lib/logic/errors.ts';
import { LANGS } from '../src/lib/i18n/langs.ts';
import { dict } from '../src/lib/i18n/dict.ts';

const run = {
	document_id: 'd1',
	accepted: 32,
	queued: 0,
	skipped: 0,
	reasons: {},
	fit: { activities: 1, heart_rate_minutes: 30, distances: 1, energy: 1, device: 'OwnHealth Test Watch', gps_left_out: true }
};

test('a run gives its counts, its device and the left-out route', () => {
	assert.deepEqual(fitLines(run), [
		{ key: 'i2_fit_activities', n: 1 },
		{ key: 'i2_fit_hr', n: 30 },
		{ key: 'i2_fit_distance', n: 1 },
		{ key: 'i2_fit_energy', n: 1 },
		{ key: 'i2_fit_device', vars: { device: 'OwnHealth Test Watch' } },
		{ key: 'i2_fit_gps' }
	]);
});

test('the same file again, a ride and an unknown reason say why samples were left out', () => {
	const again = { ...run, accepted: 0, reasons: { duplicate: 32 }, fit: { ...run.fit, heart_rate_minutes: 0, distances: 0, energy: 0 } };
	const lines = fitLines(again) ?? [];
	assert.deepEqual(lines.slice(1, 5), [
		{ key: 'i2_fit_hr', n: 0 },
		{ key: 'i2_fit_distance', n: 0 },
		{ key: 'i2_fit_energy', n: 0 },
		{ key: 'i2_fit_r_duplicate', n: 32 }
	]);
	const ride = { ...run, reasons: { bad_time: 2, other_sport_distance: 1, zone: 3, none: 0 }, fit: { ...run.fit, gps_left_out: false } };
	assert.deepEqual(
		(fitLines(ride) ?? []).filter((l) => l.key.startsWith('i2_fit_r_') || l.key === 'i2_fit_gps'),
		[
			{ key: 'i2_fit_r_other_sport_distance', n: 1 },
			{ key: 'i2_fit_r_bad_time', n: 2 },
			{ key: 'i2_fit_r_other', n: 3, vars: { code: 'zone' } }
		]
	);
});

test('any other job result, or one hidden from a member who may not read wearables, gives nothing', () => {
	for (const r of [null, undefined, 'x', {}, { accepted: 3, reasons: {} }, { cgm_csv: {} }, { fit: null }]) {
		assert.equal(fitLines(r), null, JSON.stringify(r));
	}
});

test('a refused FIT file reads as our sentence for each reason', () => {
	const has = (k: string) => k in dict.en;
	const cases: [string, string][] = [
		['PermanentJobError: not a readable FIT file: it is cut short', 'err_fit_cut'],
		['PermanentJobError: not a readable FIT file: its checksum does not match, so it is damaged', 'err_fit_damaged'],
		['PermanentJobError: not a readable FIT file: the gzip data is damaged', 'err_fit_damaged'],
		['PermanentJobError: not a readable FIT file: it is damaged (No definition found)', 'err_fit_damaged'],
		['PermanentJobError: the FIT file holds no activity: no heart rate, distance or energy', 'err_fit_empty'],
		['PermanentJobError: the FIT file is larger than 32 MB once unpacked: this is not one activity', 'err_fit_too_big']
	];
	for (const [message, key] of cases) assert.equal(errKey(null, message, has), key, message);
});

test('every FIT text exists in every registry language, with its plural and singular', () => {
	const keys = [
		'i2_ik_fit',
		'i2_ik_fit_gz',
		'fu_k_fit',
		'i2_fit_hidden',
		'i2_dt_wearables',
		'i2_fit_device',
		'i2_fit_gps',
		'err_fit_cut',
		'err_fit_damaged',
		'err_fit_empty',
		'err_fit_too_big',
		...['i2_fit_activities', 'i2_fit_hr', 'i2_fit_distance', 'i2_fit_energy', 'i2_fit_r_other', ...FIT_REASONS.map((r) => 'i2_fit_r_' + r)].flatMap((k) => [k, k + '_one'])
	];
	for (const l of LANGS) {
		const table = l.table as Record<string, string>;
		for (const k of keys) assert.ok(table[k]?.trim(), `${l.code} ${k}`);
	}
	const en = dict.en as Record<string, string>;
	const es = dict.es as Record<string, string>;
	for (const k of keys.filter((k) => k.startsWith('i2_fit_') && !k.endsWith('_one'))) {
		assert.notEqual(en[k], es[k], k);
		if (en[k].includes('{n}')) assert.ok(es[k].includes('{n}') && !en[k + '_one'].includes('{n}'), k);
	}
});

test('the reason lines say what was left out, in en and es', () => {
	const en = dict.en as Record<string, string>;
	const es = dict.es as Record<string, string>;
	const want: [string, string, string][] = [
		['i2_fit_r_bad_time', '{n} with a time before 2000 or in the future', '{n} con una hora anterior a 2000 o en el futuro'],
		['i2_fit_r_bad_time_one', '1 with a time before 2000 or in the future', '1 con una hora anterior a 2000 o en el futuro'],
		['i2_fit_r_bad_value', '{n} values outside a plausible range', '{n} valores fuera de un rango plausible'],
		['i2_fit_r_bad_value_one', '1 value outside a plausible range', '1 valor fuera de un rango plausible'],
		['i2_fit_r_other', '{n} not imported ({code})', '{n} sin importar ({code})'],
		['i2_fit_r_other_one', '1 not imported ({code})', '1 sin importar ({code})'],
		[
			'i2_fit_r_over_limit',
			'{n} past what one file holds (7 days of heart rate, 1,000 activities): not stored',
			'{n} más allá de lo que admite un archivo (7 días de frecuencia cardiaca, 1000 actividades): sin guardar'
		],
		['i2_fit_hidden', 'What this file brought is shared only with people who can see Wearables.', 'Lo que ha traído este archivo solo lo ven quienes pueden ver Wearables.'],
		['fu_k_fit', 'FIT file', 'Archivo FIT']
	];
	for (const [k, e, s] of want) {
		assert.equal(en[k], e, `en ${k}`);
		assert.equal(es[k], s, `es ${k}`);
	}
	// an upload the server does not know names FIT and Gadgetbridge among the files it takes
	assert.match(en.err_unsupported_file, /Gadgetbridge database, a FIT activity file/);
	assert.match(es.err_unsupported_file, /base de datos de Gadgetbridge, un archivo de actividad FIT/);
});

test('a FIT document shows what its file holds, from its flags', () => {
	assert.deepEqual(fitDocLines({ fit: run.fit, import: { queued: 0 } }), fitLines({ fit: run.fit }));
	assert.equal((fitDocLines({ fit: run.fit }) ?? []).at(-1)?.key, 'i2_fit_gps');
	for (const f of [null, undefined, {}, { gadgetbridge: {} }, { fit: null }]) assert.equal(fitDocLines(f), null, JSON.stringify(f));
});

test('an import.fit result hidden from this member gets its one dim line, nothing else does', () => {
	assert.equal(fitHidden({ kind: 'import.fit', state: 'done', result: null }), true);
	for (const j of [
		undefined,
		{ kind: 'import.fit', state: 'done', result: run },
		{ kind: 'import.fit', state: 'running', result: null },
		{ kind: 'import.fit', state: 'dead', result: null },
		{ kind: 'import.gadgetbridge', state: 'done', result: null }
	]) {
		assert.equal(fitHidden(j), false, JSON.stringify(j));
	}
});

test('a finished import.fit upload links to the document its job made, nothing else does', () => {
	assert.equal(fitDocId({ kind: 'import.fit', state: 'done', result: run }), 'd1');
	for (const j of [
		undefined,
		{ kind: 'import.fit', state: 'done', result: null },
		{ kind: 'import.fit', state: 'running', result: run },
		{ kind: 'import.fit', state: 'done', result: { ...run, document_id: '' } },
		{ kind: 'import.gadgetbridge', state: 'done', result: run }
	]) {
		assert.equal(fitDocId(j), undefined, JSON.stringify(j));
	}
});
