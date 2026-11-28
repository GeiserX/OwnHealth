// The hand-entry form (ManualEntry.svelte) reads every field through fieldText and fieldNumber.
// Svelte's bind:value gives a string for a text or date input, but a number (or null when empty) for a
// number input, so a vaccine's dose number reaches the form as a number. A correction pre-fills every
// field as text, and a field the person never touched is undefined.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fieldNumber, fieldText } from '../src/lib/logic/manual.ts';

test('a number from a number input is read, not called .trim() on', () => {
	assert.equal(fieldText(1), '1');
	assert.equal(fieldNumber(1), 1);
	assert.equal(fieldNumber(12), 12);
});

test('0 is a number, not an empty field', () => {
	assert.equal(fieldText(0), '0');
	assert.equal(fieldNumber(0), 0);
	assert.equal(fieldNumber('0'), 0);
});

test('an empty field is null, however the input left it', () => {
	for (const v of [null, undefined, '', '   ']) {
		assert.equal(fieldText(v), null);
		assert.equal(fieldNumber(v), null);
	}
});

test('text is trimmed, and a correction pre-filled as text still sends a number', () => {
	assert.equal(fieldText('  Tetanus '), 'Tetanus');
	assert.equal(fieldNumber('3'), 3);
	assert.equal(fieldNumber(' 2 '), 2);
});

test('text that is not a number sends no number', () => {
	assert.equal(fieldNumber('abc'), null);
	assert.equal(fieldNumber(Number.NaN), null);
});

// The form itself reads its fields through these two, for every kind and for a correction.
import { readFileSync } from 'node:fs';

test('ManualEntry reads its fields through fieldText and its number fields through fieldNumber', () => {
	const src = readFileSync(new URL('../src/lib/components/ManualEntry.svelte', import.meta.url), 'utf8');
	assert.match(src, /const val = \(k: string\) => fieldText\(f\[k\]\);/);
	assert.match(src, /type === 'number' \? fieldNumber\(f\[k\]\) : val\(k\)/);
	assert.doesNotMatch(src, /\.trim\(\) \|\| null/);
});
