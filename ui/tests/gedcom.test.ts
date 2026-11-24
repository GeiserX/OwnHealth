// GEDCOM import (slice 24): the browser reads the names for the picker from the person's own file and
// numbers people exactly as the server does, a proposal reads in es and en, and every refusal the server
// can give has its sentence in both languages.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dict } from '../src/lib/i18n/dict.ts';
import { cleanName, decodeTree, namesByPosition, pickable, proposalTitle } from '../src/lib/logic/gedcom.ts';

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
	assert.equal(cleanName(' / / '), null);
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
