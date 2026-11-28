// An Apple Health zip that holds two main export files is refused by the import; the reader gets the
// reason in their own language, not the server's English sentence.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { errKey } from '../src/lib/logic/errors.ts';
import { dict } from '../src/lib/i18n/dict.ts';

test('a zip with two Apple Health exports says so in the reader language', () => {
	const has = (k: string) => k in dict.en;
	const message =
		'the zip holds 2 Apple Health exports (apple_health_export/export.xml, apple_health_export/exportación.xml): import each export on its own';
	assert.equal(errKey(null, message, has), 'err_apple_two_exports');
	for (const lang of ['en', 'es'] as const) assert.ok(dict[lang].err_apple_two_exports, lang);
	// other Apple Health refusals keep the server's message
	assert.equal(errKey(null, 'no export.xml in the zip', has), null);
});
