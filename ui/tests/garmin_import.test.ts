// What a Garmin account export import shows under the uploaded file (slice 30): the files it read, the
// readings they added or that nothing was new, a pill that says when nothing came in, and the line for a
// member who may not see Wearables. A cut-short ZIP reads as its own sentence, not as an unknown file.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { garminNothing, garminReadings, garminSummary } from '../src/lib/logic/garmin_import.ts';
import { fitHidden } from '../src/lib/logic/fit_import.ts';
import { errKey } from '../src/lib/logic/errors.ts';
import { dict } from '../src/lib/i18n/dict.ts';

const garmin = { files_imported: 2, gps_left_out: false, not_imported: [{ member: 'a.txt', reason: 'unsupported' }] };

test('the readings line says what came in, or that every reading was already here', () => {
	assert.deepEqual(garminReadings({ accepted: 429, skipped: 0, reasons: {}, garmin }), { key: 'i2_garmin_readings', n: 429 });
	assert.deepEqual(garminReadings({ accepted: 0, skipped: 429, reasons: { duplicate: 429 }, garmin }), { key: 'i2_garmin_nothing_new', n: 429 });
	assert.equal(garminReadings({ accepted: 0, skipped: 0, reasons: {}, garmin }), null);
	assert.equal(garminReadings(null), null);
	for (const lang of ['en', 'es'] as const) {
		for (const key of ['i2_garmin_readings', 'i2_garmin_readings_one', 'i2_garmin_nothing_new', 'i2_garmin_nothing_new_one', 'i2_garmin_nothing', 'i2_garmin_reason_limit', 'fu_k_garmin_export', 'err_zip_unreadable']) {
			assert.ok((dict[lang] as Record<string, string>)[key], `${lang} ${key}`);
		}
	}
});

test('an import that read no file says nothing was imported', () => {
	const done = (files_imported: number) => ({ kind: 'import.garmin_zip', state: 'done', result: { accepted: 0, garmin: { ...garmin, files_imported } } });
	assert.equal(garminNothing(done(0)), true);
	assert.equal(garminNothing(done(2)), false);
	assert.equal(garminNothing({ kind: 'import.garmin_zip', state: 'done', result: null }), false);
	assert.equal(garminNothing({ kind: 'import.fit', state: 'done', result: { garmin: { ...garmin, files_imported: 0 } } }), false);
	assert.equal(garminNothing(undefined), false);
	assert.deepEqual(garminSummary({ ...garmin, gps_left_out: 'yes' }), { ...garmin, gps_left_out: false });
	assert.equal(garminSummary({ files_imported: '2', not_imported: [] }), null);
});

test('a hidden Garmin result gets the Wearables line, as a FIT one does', () => {
	assert.equal(fitHidden({ kind: 'import.garmin_zip', state: 'done', result: null }), true);
	assert.equal(fitHidden({ kind: 'import.garmin_zip', state: 'done', result: { garmin } }), false);
	assert.equal(fitHidden({ kind: 'import.garmin_zip', state: 'running', result: null }), false);
});

test('a ZIP whose member list cannot be read is no unknown file', () => {
	const has = (k: string) => k in dict.en;
	assert.equal(errKey(null, 'garmin.zip: this ZIP could not be read; it may be incomplete', has), 'err_zip_unreadable');
	assert.equal(errKey(null, 'x.zip: not a PDF, image, Apple Health export, FHIR bundle or genome facts file', has), 'err_unsupported_file');
});
