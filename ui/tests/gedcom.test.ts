// GEDCOM import (slice 24): the browser reads the names for the picker from the person's own file and
// numbers people exactly as the server does, a proposal reads in es and en, and every refusal the server
// can give has its sentence in both languages.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dict } from '../src/lib/i18n/dict.ts';
import { cleanName, decodeTree, doneLine, namesByPosition, pickable, proposalTitle } from '../src/lib/logic/gedcom.ts';

const tables = dict as unknown as Record<string, Record<string, string>>;
const f551 = readFileSync(new URL('../../fixtures/gedcom/rivera-551.ged', import.meta.url));
const f70 = readFileSync(new URL('../../fixtures/gedcom/rivera-70.ged', import.meta.url));
const server = readFileSync(new URL('../../server/crates/ownhealth-core/src/gedcom.rs', import.meta.url), 'utf8');
const word = (lang: string) => (key: string, vars: Record<string, string | number> = {}) =>
	(tables[lang][key] ?? key).replace(/\{(\w+)\}/g, (_, k) => String(vars[k] ?? `{${k}}`));

test('names are read by position, the way the server numbers people', () => {
	const names = namesByPosition(decodeTree(new Uint8Array(f551)));
	// the server's tests: 19 people, Alex at position 0, adopted Pablo at 8
	assert.equal(names.length, 19);
	assert.equal(names[0], 'Alex Rivera Vexlorn');
	assert.equal(names[8], 'Pablo Rivera Vexlorn');
	// GEDCOM 7, the maximal70 shape: six INDI records, one without an xref; @I4@ has no NAME
	const seven = namesByPosition(decodeTree(new Uint8Array(f70)));
	assert.equal(seven.length, 6);
	assert.equal(seven[0], 'Alex Rivera Vexlorn');
	assert.equal(seven[3], null);
});

test('line endings and UTF-16 give the same names', () => {
	const text = decodeTree(new Uint8Array(f551));
	const want = namesByPosition(text);
	assert.deepEqual(namesByPosition(text.replace(/\n/g, '\r\n')), want);
	assert.deepEqual(namesByPosition(text.replace(/\n/g, '\r')), want);
	const le = new Uint8Array(2 + text.length * 2);
	le.set([0xff, 0xfe]);
	for (let i = 0; i < text.length; i++) {
		const c = text.charCodeAt(i);
		le[2 + i * 2] = c & 0xff;
		le[3 + i * 2] = c >> 8;
	}
	assert.deepEqual(namesByPosition(decodeTree(le)), want);
	assert.equal(cleanName('Alex /Rivera/  jr.'), 'Alex Rivera jr.');
	// the first NAME of a record is the one shown, not a later one
	assert.deepEqual(namesByPosition('0 HEAD\n0 @I1@ INDI\n1 NAME Alex /Rivera/\n1 NAME Alejandro /Vexlorn/\n0 TRLR\n'), ['Alex Rivera']);
	assert.equal(cleanName(' / / '), null);
});

/** A 5.5.1 tree in one byte per character, its `1 CHAR` line saying which set. */
function oneByte(charset: string, name: number[]): Uint8Array {
	const ascii = (s: string) => [...s].map((c) => c.charCodeAt(0));
	return new Uint8Array([
		...ascii(`0 HEAD\r\n1 GEDC\r\n2 VERS 5.5.1\r\n1 CHAR ${charset}\r\n0 @I1@ INDI\r\n1 NAME `),
		...name,
		...ascii('\r\n1 SEX F\r\n0 TRLR\r\n')
	]);
}

test('a tree saved as ANSI or ANSEL shows its accented names', () => {
	// Ilsabet /Núñez/ in Windows-1252: ú = 0xFA, ñ = 0xF1
	const ansi = [...'Ilsabet /N'].map((c) => c.charCodeAt(0)).concat([0xfa, 0xf1, 0x65, 0x7a, 0x2f]);
	assert.deepEqual(namesByPosition(decodeTree(oneByte('ANSI', ansi))), ['Ilsabet Núñez']);
	// the same bytes with no CHAR line that names them: not UTF-8, so Windows-1252
	assert.deepEqual(namesByPosition(decodeTree(oneByte('UTF-8', ansi))), ['Ilsabet Núñez']);
	// ANSEL puts the accent before its letter: 0xE2 acute, 0xE4 tilde; 0xA2 is Ø
	const ansel = [...'Ilsabet /N'].map((c) => c.charCodeAt(0)).concat([0xe2, 0x75, 0xe4, 0x6e, 0x65, 0x7a, 0x2f, 0x20, 0xa2, 0x79]);
	assert.deepEqual(namesByPosition(decodeTree(oneByte('ANSEL', ansel))), ['Ilsabet Núñez Øy']);
	// a file that says ANSEL but is UTF-8 stays UTF-8
	const utf8 = new TextEncoder().encode('0 HEAD\n1 CHAR ANSEL\n0 @I1@ INDI\n1 NAME Ilsabet /Núñez/\n0 TRLR\n');
	assert.deepEqual(namesByPosition(decodeTree(utf8)), ['Ilsabet Núñez']);
});

test('one stray byte in a UTF-8 tree keeps every other accented name', () => {
	// `1 CHAR UTF-8`, Núñez in UTF-8, and one Windows-1252 é (0xE9) in another name
	const enc = (s: string) => [...new TextEncoder().encode(s)];
	const mixed = new Uint8Array([
		...enc('0 HEAD\n1 CHAR UTF-8\n0 @I1@ INDI\n1 NAME Ilsabet /Núñez/\n0 @I2@ INDI\n1 NAME Ren'),
		0xe9,
		...enc(' /Vexlorn/\n0 TRLR\n')
	]);
	assert.deepEqual(namesByPosition(decodeTree(mixed)), ['Ilsabet Núñez', 'René Vexlorn']);
	// the same with CRLF line ends and no CHAR line
	const crlf = new Uint8Array([
		...enc('0 HEAD\r\n0 @I1@ INDI\r\n1 NAME Ilsabet /Núñez/\r\n0 @I2@ INDI\r\n1 NAME Ren'),
		0xe9,
		...enc(' /Vexlorn/\r\n0 TRLR\r\n')
	]);
	assert.deepEqual(namesByPosition(decodeTree(crlf)), ['Ilsabet Núñez', 'René Vexlorn']);
});

test('the picker finds by name or birth year, and shows no names when the counts differ', () => {
	const people = [
		{ i: 0, sex: 'male', born_year: 1985, deceased: false, death_age: null },
		{ i: 1, sex: 'male', born_year: 1955, deceased: true, death_age: 64 },
		{ i: 2, sex: 'female', born_year: 1958, deceased: false, death_age: null }
	];
	const names = ['Alex Rivera', 'Tomás Rivera', 'Marta Ortega'];
	assert.deepEqual(pickable(people, names, 'tom').map((p) => p.i), [1]);
	assert.deepEqual(pickable(people, names, '195').map((p) => p.i), [1, 2]);
	assert.deepEqual(pickable(people, names, '').map((p) => p.i), [0, 1, 2]);
	assert.deepEqual(pickable(people, names, '', 2).map((p) => p.i), [0, 1]);
	// a file the browser counted differently: names are not trusted, years still work
	assert.deepEqual(pickable(people, names.slice(1), 'marta'), []);
	assert.deepEqual(pickable(people, names.slice(1), '1958').map((p) => p.i), [2]);
});

test('a proposal reads in es and en, with no name', () => {
	const p = { relationship: 'FTH', label: { es: 'padre', en: 'father' }, sex_at_birth: 'male', born_year: 1955, deceased: true, death_age: 64 };
	assert.equal(proposalTitle(p, 'en', word('en')), 'Father · born in 1955 · died at 64');
	assert.equal(proposalTitle(p, 'es', word('es')), 'Padre · nacimiento en 1955 · murió a los 64 años');
	assert.equal(proposalTitle({ ...p, death_age: null }, 'en', word('en')), 'Father · born in 1955 · died');
	assert.equal(proposalTitle({ label: { es: 'madre', en: 'mother' }, deceased: false }, 'es', word('es')), 'Madre');
	assert.equal(proposalTitle({}, 'en', word('en')), 'Relative from a family tree');
	// a relationship that names no sex shows the sex the relative will be recorded with
	const sps = { relationship: 'SPS', label: { es: 'pareja', en: 'spouse or partner' }, sex_at_birth: 'female', born_year: 1987, deceased: false, death_age: null };
	assert.equal(proposalTitle(sps, 'en', word('en')), 'Spouse or partner · female · born in 1987');
	assert.equal(proposalTitle(sps, 'es', word('es')), 'Pareja · mujer · nacimiento en 1987');
	const cousin = { relationship: 'PCOUSN', label: { es: 'primo o prima por parte de padre', en: 'paternal cousin' }, sex_at_birth: 'unknown', born_year: 1984 };
	assert.equal(proposalTitle(cousin, 'en', word('en')), 'Paternal cousin · sex unknown · born in 1984');
	assert.equal(proposalTitle({ ...cousin, sex_at_birth: 'male' }, 'es', word('es')), 'Primo o prima por parte de padre · hombre · nacimiento en 1984');
});

test('the end of an import counts the new proposals, and says so when there are none', () => {
	assert.equal(doneLine(0, word('en')), 'No new relatives: all of them are already there.');
	assert.equal(doneLine(0, word('es')), 'Ningún familiar nuevo: ya estaban todos.');
	assert.equal(doneLine(1, word('en')), '1 relative waits in Import & review as a proposal.');
	assert.equal(doneLine(3, word('es')), '3 familiares esperan en Importar y revisar como propuestas.');
});

test('every refusal code the server gives has its sentence in es and en', () => {
	const body = server.slice(server.indexOf('pub fn message'), server.indexOf('/// Every refusal text'));
	const codes = [...body.matchAll(/"([a-z_]+)" =>/g)].map((m) => m[1]);
	assert.ok(codes.length >= 9, codes.join(','));
	for (const code of [...codes, 'other']) {
		for (const lang of ['en', 'es']) assert.ok(tables[lang]['gd_bad_' + code], `${lang} gd_bad_${code}`);
	}
	for (const key of Object.keys(tables.en).filter((k) => k.startsWith('gd_'))) assert.ok(tables.es[key], `es ${key}`);
});
