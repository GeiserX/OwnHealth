// A stored file that is gone (a held mail's message, a document's file) answers 404 `blob_missing`:
// every page that shows errors through errText says it in the person's language, never in the
// server's English.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { errKey } from '../src/lib/logic/errors.ts';
import { LANGS } from '../src/lib/i18n/langs.ts';
import { dict } from '../src/lib/i18n/dict.ts';

test('a missing stored file reads as our sentence, not the server text', () => {
	const has = (k: string) => k in dict.en;
	assert.equal(errKey('blob_missing', 'A stored file is missing.', has), 'err_blob_missing');
});

test('the missing-file sentence exists in every registry language', () => {
	const words = LANGS.map((l) => (l.table as Record<string, string>).err_blob_missing);
	for (const [i, w] of words.entries()) assert.ok(w && w.trim(), `${LANGS[i].code}: no words`);
	assert.notEqual(dict.es.err_blob_missing, dict.en.err_blob_missing, 'es is the English text');
	assert.notEqual(dict.es.err_blob_missing, 'A stored file is missing.');
});
