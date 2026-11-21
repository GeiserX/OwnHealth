// SMART Health Card receive (slice 22): every refusal reason has a sentence in both languages, and
// the receiver never calls a signer trusted or verified.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { SHC_REASONS, refusalKey } from '../src/lib/logic/shc.ts';
import { dict } from '../src/lib/i18n/dict.ts';

const tables = dict as unknown as Record<string, Record<string, string>>;

test('every refusal reason has an es and an en sentence, and an unknown one gets the general one', () => {
	for (const lang of ['en', 'es']) {
		for (const r of SHC_REASONS) assert.ok(tables[lang][refusalKey(r)], `${lang} ${r}`);
		assert.equal(refusalKey('something_new'), 'shc_r_other');
		assert.ok(tables[lang].shc_r_other);
	}
	assert.equal(refusalKey('bad_signature'), 'shc_r_bad_signature');
});

test('the receiver says who signed a card, never that the signer is trusted or verified', () => {
	const shc = (lang: string) => Object.entries(tables[lang]).filter(([k]) => k.startsWith('shc_'));
	for (const lang of ['en', 'es']) {
		assert.ok(shc(lang).length > 30, lang);
		for (const [k, v] of shc(lang)) {
			assert.doesNotMatch(v, /trust|verified|confianza|confiable|verificad/i, `${lang} ${k}: ${v}`);
		}
	}
	assert.equal(tables.en.shc_signed_by, 'Signed by');
	assert.equal(tables.es.shc_signed_by, 'Firmada por');
});
