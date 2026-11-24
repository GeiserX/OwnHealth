// Slice 43: a held study of an upload that was removed, an upload removed while it was saved, a
// study whose images a rebuild is still sending back to the image store, and a study of a deleted
// profile a restore brought back answer codes of their own. The page says them in the person's
// language, never in the server's English. The profile delete line says where the scans go.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { errKey } from '../src/lib/logic/errors.ts';
import { LANGS } from '../src/lib/i18n/langs.ts';
import { dict } from '../src/lib/i18n/dict.ts';

const has = (k: string) => k in dict.en;

for (const code of ['upload_removed', 'upload_lost', 'study_rebuilding', 'study_closed']) {
	test(`${code} reads as our sentence in every registry language`, () => {
		assert.equal(errKey(code, 'Some English text from the server.', has), `err_${code}`);
		for (const l of LANGS) {
			const w = (l.table as Record<string, string>)[`err_${code}`];
			assert.ok(w && w.trim(), `${l.code}: no words`);
		}
		const t = (lang: 'en' | 'es') => (dict[lang] as Record<string, string>)[`err_${code}`];
		assert.notEqual(t('es'), t('en'), 'es is the English text');
	});
}

test('the profile delete line says the scans leave the image store too', () => {
	assert.match(dict.en.st_delete_d, /scans/);
	assert.match(dict.es.st_delete_d, /imágenes médicas/);
});
