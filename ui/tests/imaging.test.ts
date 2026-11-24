// The Imaging page (slice 42): what a DICOM import did, the import API's refusals as our own sentences
// in every registry language, and where focus goes on /import-review after a decision removes a row.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { focusOrder, imagingLines } from '../src/lib/logic/imaging.ts';
import { errKey } from '../src/lib/logic/errors.ts';
import { LANGS } from '../src/lib/i18n/langs.ts';

test('a finished import names what it put on the record and leaves out the zero counts', () => {
	const result = { files: 9, studies: 3, imported: 1, already: 1, in_review: 1, refused: 0, damaged: 2, instances: 6 };
	assert.deepEqual(imagingLines(result), [
		{ key: 'img_r_imported', n: 1 },
		{ key: 'img_r_already', n: 1 },
		{ key: 'img_r_in_review', n: 1 },
		{ key: 'img_r_damaged', n: 2 }
	]);
	// nothing new still says so
	assert.deepEqual(imagingLines({ imported: 0 }), [{ key: 'img_r_imported', n: 0 }]);
	// a result hidden from this member (no read:records) shows no lines
	assert.equal(imagingLines(null), null);
});

for (const { code: lang, table: words } of LANGS) {
	const table = words as Record<string, string>;
	const has = (k: string) => k in table;
	test(`the import API's refusals read as our sentence in ${lang}, never the server's English`, () => {
		for (const [code, message] of [
			['too_many_files', 'This zip holds 100001 files; an import reads at most 100000. Split the disc into smaller zips.'],
			['invalid_zip', 'This zip does not open: invalid Zip archive'],
			['imaging_off', 'Imaging is off: it needs the imaging compose profile and OWNHEALTH_ORTHANC_URL.'],
			['profile_changed', 'The profile changed during the upload. Upload the disc again.'],
			['study_not_bound', 'Orthanc does not hold this study as recorded.']
		]) {
			const key = errKey(code, message, has);
			assert.equal(key, 'err_' + code, code);
			assert.ok(table[key ?? ''] && table[key ?? ''] !== message, `${lang} ${code}`);
		}
		for (const k of ['img_r_imported', 'img_r_imported_one', 'img_r_already', 'img_r_in_review', 'img_r_refused', 'img_r_damaged', 'img_none', 'img_open_report', 'img_no_report']) {
			assert.ok(table[k], `${lang} ${k}`);
		}
	});
}

test('after a decision, focus tries the row itself, then the rows below, then the rows above', () => {
	const rows = ['a', 'b', 'c', 'd'];
	assert.deepEqual(focusOrder(rows, 'b'), ['b', 'c', 'd', 'a']);
	assert.deepEqual(focusOrder(rows, 'd'), ['d', 'c', 'b', 'a']);
	assert.deepEqual(focusOrder(rows, 'a'), ['a', 'b', 'c', 'd']);
	assert.deepEqual(focusOrder(rows, 'x'), []);
});
