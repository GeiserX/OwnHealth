// The language registry (src/lib/i18n/langs.ts): every listed language has every UI string, a
// missing one fails here (and in `pnpm run check`, through the Table type), and the picker, number
// formats and plural rules read the registry instead of assuming es/en.
import { test } from 'node:test';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import assert from 'node:assert/strict';
import { BROWSER_DEFAULT, LANGS, intlLocale, isLang, missingKeys, pickLang, type Table } from '../src/lib/i18n/langs.ts';
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

// A pick of es or en written into a component: a comparison with the code either way round and in
// either quote, a two-language type, a per-language field (`summary_es`, `en?:`), a hand-written
// option (`value="es"`) or a hand-written list row (`['es', 'Español']`). Screens read the registry
// (LANGS, Lang, Localized) instead.
const BY_HAND =
	/[!=]==? ?['"]e[sn]['"]|['"]e[sn]['"] ?[!=]==?|['"]e[sn]['"] \| ['"]e[sn]['"]|\?\? ?['"]e[sn]['"]|_es\b|_en\b|value="e[sn]"|\[['"]e[sn]['"], |\be[sn]\?:/;
// Each allowed line, with its reason. An entry with a line allows exactly that line (trimmed); one
// without allows the whole folder. An entry that allows nothing fails the scan.
const ALLOWED: [string, string][] = [
	// the registry and the en/es dictionaries themselves
	['lib/i18n/', ''],
	// the server writes a review detail in English: shown as a second part to an English reader only
	[
		'lib/components/pages2/ReviewRow.svelte',
		"i18n.lang === 'en' && c.detail && label(c) && !isCode(c.detail) && !(c.reason === 'conflict' && c.value) && !repeatsLabel(c.detail, enLabel(c)) && !/\\([0-9a-f-]{36}\\)/.test(c.detail) ? c.detail : '';"
	],
	// a Spanish typographic rule (lowercase after a colon), not a language pick
	['lib/logic/format.ts', "if (lang !== 'es' || s.length < 2 || s[1] !== s[1].toLocaleLowerCase('es')) return s;"]
];

function sources(dir: string): string[] {
	return readdirSync(dir).flatMap((n) => {
		const p = join(dir, n);
		return statSync(p).isDirectory() ? sources(p) : /\.(ts|svelte)$/.test(n) ? [p] : [];
	});
}

const allowedBy = (name: string, line: string) => ALLOWED.findIndex(([f, l]) => name.startsWith(f) && (!l || line.trim() === l));

function picksByHand(root: string): { picks: string[]; stale: string[] } {
	const picks: string[] = [];
	const used = new Set<number>();
	for (const file of sources(root)) {
		const name = relative(root, file);
		readFileSync(file, 'utf8')
			.split('\n')
			.forEach((line, i) => {
				const code = line.replace(/\/\/.*$/, '');
				if (!BY_HAND.test(code)) return;
				const k = allowedBy(name, line);
				if (k >= 0) used.add(k);
				else picks.push(`${name}:${i + 1}: ${line.trim()}`);
			});
	}
	return { picks, stale: ALLOWED.filter((_, k) => !used.has(k)).map(([f, l]) => `${f}: ${l}`) };
}

test('no screen picks es or en by hand: every language choice reads the registry', () => {
	const { picks, stale } = picksByHand(join(import.meta.dirname, '../src'));
	assert.deepEqual(picks, []);
	assert.deepEqual(stale, [], 'ALLOWED entries that match no line');
});

test('control: the scan sees each form of a pick', () => {
	for (const pick of [
		"const lang = $derived(i18n.lang === 'es' ? 'es' : 'en');",
		"let lang = $state<'es' | 'en'>(i18n.lang);",
		"return (lang === 'es' ? s('summary_es') : undefined) ?? s('summary');",
		"const x = v[lang] ?? v['en'] ?? 'en';",
		'const x = i18n.lang == "es" ? "es" : "en";',
		"if ('en' !== i18n.lang) return;",
		'<option value="es">Español</option><option value="en">English</option>',
		"const LETTER_LANGS = [['es', 'Español'], ['en', 'English']];",
		"['en', 'English']",
		'scopes: { scope: Scope; en?: string; es?: string }[];'
	])
		assert.ok(BY_HAND.test(pick), pick);
	for (const fine of [
		'let lang = $state<Lang>(i18n.lang);',
		"s(`summary_${lang}`)",
		'const enLabel = 1;',
		'{#each LANGS as l (l.code)}<option value={l.code}>{l.name}</option>{/each}',
		'const seen?: boolean;'
	])
		assert.ok(!BY_HAND.test(fine), fine);
	// an allowed line allows only itself
	assert.ok(allowedBy('lib/logic/format.ts', "\tif (lang !== 'es' || s.length < 2 || s[1] !== s[1].toLocaleLowerCase('es')) return s;") >= 0);
	assert.equal(allowedBy('lib/logic/format.ts', "if (lang !== 'es') return s;"), -1);
	assert.equal(allowedBy('lib/logic/other.ts', "if (lang !== 'es' || s.length < 2 || s[1] !== s[1].toLocaleLowerCase('es')) return s;"), -1);
});

test('a browser language the registry lacks reads the shared default, the server one too', () => {
	assert.equal(pickLang(null, null, 'ja-JP'), BROWSER_DEFAULT);
	assert.ok(isLang(BROWSER_DEFAULT));
});
