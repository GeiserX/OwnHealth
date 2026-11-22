// The audit card's lines about the log itself (GET /api/audit/chain): a held chain, a broken entry, and
// the head the last backup kept outside the database, in both languages, never with a {placeholder} left.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dict } from '../src/lib/i18n/dict.ts';
import { chainLines } from '../src/lib/logic/people.ts';

const fill = (lang: 'en' | 'es', key: string, vars: Record<string, string | number>) => {
	const text = (dict[lang] as Record<string, string>)[key];
	assert.ok(text, `${lang} has ${key}`);
	return text.replace(/\{(\w+)\}/g, (_, k) => (k in vars ? String(vars[k]) : `{${k}}`));
};

const anchor = { state: 'ok', seq: 40, at: '2026-11-22' };

test('a held chain with a matching anchor reads as two OK lines', () => {
	const lines = chainLines({ verified: 42, broken: null, anchor });
	assert.deepEqual(
		lines.map((l) => [l.key, l.tone]),
		[
			['au_chain_ok', 'p-ok'],
			['au_anchor_ok', 'p-ok']
		]
	);
	assert.equal(fill('en', lines[0].key, lines[0].vars), 'Every entry is verified, up to entry 42.');
	assert.match(fill('es', lines[1].key, lines[1].vars), /entrada 40 .*del 2026-11-22/);
});

test('a changed entry turns the chain line into a problem that names it', () => {
	const [chain] = chainLines({ verified: 6, broken: { seq: 7, kind: 'hash' }, anchor });
	assert.deepEqual([chain.key, chain.tone, chain.vars], ['au_chain_broken_hash', 'p-out', { n: '6', seq: '7' }]);
	assert.equal(fill('en', chain.key, chain.vars), 'Entry 7 was changed after it was written. Entries up to 6 are verified.');
});

test('a rewritten log that holds again never reads as verified while the anchor does not match', () => {
	for (const state of ['mismatch', 'unreadable']) {
		const lines = chainLines({ verified: 42, broken: null, anchor: { ...anchor, state } });
		assert.deepEqual(
			lines.map((l) => [l.key, l.tone]),
			[
				['au_chain_unanchored', 'p-out'],
				[`au_anchor_${state}`, 'p-out']
			],
			state
		);
		for (const lang of ['en', 'es'] as const) assert.doesNotMatch(fill(lang, lines[0].key, lines[0].vars), /^(Every|Todas)/, `${state} ${lang}`);
	}
	const [, mismatch] = chainLines({ verified: 42, broken: null, anchor: { ...anchor, state: 'mismatch' } });
	const text = fill('en', mismatch.key, { ...mismatch.vars, at: '2026-11-22' });
	assert.match(text, /no longer matches entry 40 /);
	assert.match(text, /restored from an older backup/, 'a restore of an older backup is named as a cause');
	assert.match(text, /rewrote or removed entries/);
	assert.match(text, /restored after 2026-11-22/, 'it says what to check');
});

test('an empty log reads as empty, never as verified', () => {
	const [chain] = chainLines({ verified: 0, broken: null, anchor: { state: 'none', seq: null, at: null } });
	assert.deepEqual([chain.key, chain.tone], ['au_chain_empty', 'p-info']);
});

test('the entry numbers go through the page number formatter', () => {
	const num = (n: number) => n.toLocaleString('en-US');
	const lines = chainLines({ verified: 100049, broken: null, anchor: { ...anchor, seq: 100000 } }, num);
	assert.equal(fill('en', lines[0].key, lines[0].vars), 'Every entry is verified, up to entry 100,049.');
	assert.match(fill('en', lines[1].key, lines[1].vars), /entry 100,000 /);
	const [broken] = chainLines({ verified: 1999, broken: { seq: 2000, kind: 'hash' }, anchor }, num);
	assert.deepEqual(broken.vars, { n: '1,999', seq: '2,000' });
});

test('a log that cannot be checked says the anchor cannot be compared either', () => {
	const lines = chainLines({ verified: 0, broken: { seq: null, kind: 'uncheckable' }, anchor: { ...anchor, state: 'unchecked' } });
	assert.deepEqual(
		lines.map((l) => [l.key, l.tone]),
		[
			['au_chain_uncheckable', 'p-out'],
			['au_anchor_unchecked', 'p-out']
		]
	);
});

test('with no anchor the line says none is kept now, not that no backup ever kept one', () => {
	const [, none] = chainLines({ verified: 3, broken: null, anchor: { state: 'none', seq: null, at: null } });
	assert.equal(none.key, 'au_anchor_none');
	assert.doesNotMatch(fill('en', none.key, none.vars), /No backup has/);
	assert.match(fill('en', none.key, none.vars), /right now/);
});

test('every state has words in en and es with no placeholder left', () => {
	const cases = [
		{ verified: 0, broken: null, anchor: { state: 'none', seq: null, at: null } },
		{ verified: 0, broken: { seq: null, kind: 'uncheckable' }, anchor: { state: 'unreadable', seq: null, at: null } },
		{ verified: 0, broken: { seq: null, kind: 'uncheckable' }, anchor: { ...anchor, state: 'unchecked' } },
		{ verified: 5, broken: null, anchor: { ...anchor, state: 'mismatch' } },
		...['hash', 'prev_hash', 'missing_link', 'missing_event', 'unlinked'].map((kind) => ({
			verified: 3,
			broken: { seq: kind === 'unlinked' ? null : 4, kind },
			anchor
		}))
	];
	for (const c of cases) {
		for (const l of chainLines(c)) {
			for (const lang of ['en', 'es'] as const) {
				const text = fill(lang, l.key, { ...l.vars, at: '2026-11-22' });
				assert.ok(!/\{\w+\}/.test(text), `${lang} ${l.key}: ${text}`);
			}
		}
	}
});
