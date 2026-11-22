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
	assert.deepEqual([chain.key, chain.tone, chain.vars], ['au_chain_broken_hash', 'p-out', { n: 6, seq: 7 }]);
	assert.equal(fill('en', chain.key, chain.vars), 'Entry 7 was changed after it was written. Entries up to 6 are verified.');
});

test('a rewritten log that holds again still shows the anchor mismatch', () => {
	const lines = chainLines({ verified: 42, broken: null, anchor: { ...anchor, state: 'mismatch' } });
	assert.deepEqual(
		lines.map((l) => l.tone),
		['p-ok', 'p-out']
	);
	assert.match(fill('en', lines[1].key, lines[1].vars), /no longer matches entry 40 .*Backups stop/);
});

test('every state has words in en and es with no placeholder left', () => {
	const cases = [
		{ verified: 0, broken: null, anchor: { state: 'none', seq: null, at: null } },
		{ verified: 0, broken: { seq: null, kind: 'uncheckable' }, anchor: { state: 'unreadable', seq: null, at: null } },
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
