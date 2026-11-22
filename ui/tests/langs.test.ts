// The language registry (src/lib/i18n/langs.ts): every listed language has every UI string, a
// missing one fails here (and in `pnpm run check`, through the Table type), and the picker, number
// formats and plural rules read the registry instead of assuming es/en.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { LANGS, intlLocale, isLang, missingKeys, pickLang, type Table } from '../src/lib/i18n/langs.ts';
import { dict } from '../src/lib/i18n/dict.ts';
import { pluralKey } from '../src/lib/logic/plural.ts';

const enKeys = Object.keys(dict.en);

test('every registry language has every UI string, and no other', () => {
	for (const l of LANGS) assert.deepEqual(missingKeys(l.table), { missing: [], empty: [], extra: [] }, l.code);
});

test('control: a dummy language with one string missing, one empty and one extra is caught', () => {
	const [gone, blank] = enKeys;
	const dummy: Record<string, string> = { ...dict.en, [blank]: ' ', not_a_key: 'x' };
	delete dummy[gone];
	assert.deepEqual(missingKeys(dummy as unknown as Table), { missing: [gone], empty: [blank], extra: ['not_a_key'] });
});

test('codes are unique two-letter codes with a label, a name and a valid Intl locale', () => {
	assert.deepEqual(
		LANGS.map((l) => l.code).slice(0, 2),
		['es', 'en']
	);
	assert.equal(new Set(LANGS.map((l) => l.code)).size, LANGS.length);
	for (const l of LANGS) {
		assert.match(l.code, /^[a-z]{2}$/);
		assert.equal(l.label, l.code.toUpperCase());
		assert.ok(l.name.trim());
		assert.deepEqual(Intl.getCanonicalLocales(l.intl), [l.intl]);
		assert.equal(intlLocale(l.code), l.intl);
	}
	assert.equal(intlLocale('es'), 'es-ES');
	assert.equal(intlLocale('en'), 'en-GB');
});

test('the first language comes from the URL, then the saved choice, then the browser', () => {
	assert.equal(pickLang('en', 'es', 'es-ES'), 'en');
	assert.equal(pickLang('zz', 'es', 'en-US'), 'es');
	assert.equal(pickLang(null, null, 'es-MX'), 'es');
	assert.equal(pickLang(null, 'zz', 'en-US'), 'en');
	assert.equal(pickLang(null, null, 'ja-JP'), 'en');
	assert.equal(pickLang(null, null, undefined), 'en');
	assert.ok(isLang('es') && isLang('en') && !isLang('zz') && !isLang('ES'));
});

test('plural rules follow the language, not a fixed es/en switch', () => {
	const has = () => true;
	for (const l of LANGS) assert.equal(pluralKey(l.code, 1, 'k', has), 'k_one', l.code);
	assert.equal(pluralKey('es', 2, 'k', has), 'k');
	// French says "one" for 0 too; the old switch read every other language as English.
	assert.equal(pluralKey('fr', 0, 'k', has), 'k_one');
});
