// The pedigree drawing (v0.3 family history): three generations placed in rows, couples and sibships
// joined, and the name toggle that leaves no name anywhere in the drawing's text.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { addEach, fromHousehold, layout, type Words } from '../src/lib/logic/pedigree.ts';
import type { Pedigree, PedigreeNode } from '../src/lib/api/family.ts';

const words: Words = {
	proband: 'this profile',
	masked: 'records not visible',
	died: (a) => (a == null ? '†' : `† ${a}`),
	more: (n) => `+${n} more`
};

function node(over: Partial<PedigreeNode> & Pick<PedigreeNode, 'id' | 'relationship' | 'generation' | 'slot'>): PedigreeNode {
	return {
		label: { en: over.relationship.toLowerCase(), es: over.relationship.toLowerCase() },
		side: 'own',
		degree: 1,
		union: null,
		child_of: null,
		sex_at_birth: null,
		name: null,
		age: null,
		deceased: false,
		death_age: null,
		conditions: [],
		linked: false,
		masked: false,
		src: 'src_demo',
		...over
	};
}

const mi = { kind: 'mi', text: null, en: 'heart attack', es: 'infarto de miocardio', short_en: 'MI', short_es: 'IAM', group: 'cardiovascular', onset_age: 52, onset_approx: false };

const rivera: Pedigree = {
	profile: { slug: 'alex', name: 'Alex Rivera', sex_at_birth: 'male', age: 47, deceased: false, slot: 25, union: 'self', child_of: 'parents' },
	as_of: '2026-11-10',
	rules: 'relationships.yaml 0.1.0, conditions.yaml 0.1.0',
	nodes: [
		node({ id: 'f', relationship: 'FTH', label: { en: 'father', es: 'padre' }, generation: -1, slot: 15, union: 'parents', sex_at_birth: 'male', deceased: true, death_age: 64, conditions: [mi] }),
		node({ id: 'm', relationship: 'MTH', label: { en: 'mother', es: 'madre' }, generation: -1, slot: 35, union: 'parents', sex_at_birth: 'female', name: 'Carmen Ortega', age: 71, linked: true }),
		node({ id: 's', relationship: 'SIS', label: { en: 'sister', es: 'hermana' }, generation: 0, slot: 14, child_of: 'parents', sex_at_birth: 'female', age: 44 }),
		node({ id: 'p', relationship: 'SPS', label: { en: 'spouse or partner', es: 'pareja' }, generation: 0, slot: 45, union: 'self', child_of: 'inlaws', sex_at_birth: 'female', name: 'Sam Rivera', linked: true }),
		node({ id: 'd', relationship: 'DAU', label: { en: 'daughter', es: 'hija' }, generation: 1, slot: 35, child_of: 'self', sex_at_birth: 'female', name: 'Lucía Rivera', age: 9, linked: true }),
		node({ id: 'fl', relationship: 'FTHINLAW', label: { en: 'father-in-law', es: 'suegro' }, generation: -1, slot: 60, union: 'inlaws', sex_at_birth: 'male' })
	]
};
const NAMES = ['Alex', 'Rivera', 'Carmen', 'Ortega', 'Sam', 'Lucía'];

const texts = (l: ReturnType<typeof layout>) => l.nodes.flatMap((n) => [...n.lines, n.title]).concat(l.gens.map((g) => g.label));

test('three generations in three rows, the proband under the parents and the child under the couple', () => {
	const l = layout(rivera, 'en', false, words);
	assert.deepEqual(l.gens.map((g) => g.label), ['I', 'II', 'III']);
	const ys = new Set(l.nodes.map((n) => n.y));
	assert.equal(ys.size, 3);
	const at = (id: string | null) => l.nodes.find((n) => n.id === id)!;
	const [f, m, me, sam, lucia] = [at('f'), at('m'), at(null), at('p'), at('d')];
	assert.ok(f.y < me.y && me.y < lucia.y);
	assert.equal(me.x, (f.x + m.x) / 2, 'the proband hangs from the middle of the parents');
	assert.equal(lucia.x, (me.x + sam.x) / 2, 'the daughter hangs from the middle of the couple');
	// the couple line between the parents, and a vertical line down to each child
	assert.ok(l.segs.some((s) => s.y1 === f.y && s.y2 === f.y && s.x1 === f.x + 18 && s.x2 === m.x - 18));
	assert.ok(l.segs.some((s) => s.x1 === lucia.x && s.x2 === lucia.x && s.y2 === lucia.y - 18));
	// symbols: deceased and affected father, proband marked
	assert.ok(f.deceased && f.affected && f.lines.includes('† 64') && f.lines.includes('MI 52'));
	assert.ok(me.proband && !m.affected);
	// nobody overlaps inside a row
	for (const a of l.nodes) for (const b of l.nodes) if (a !== b && a.y === b.y) assert.ok(Math.abs(a.x - b.x) >= 120, `${a.id} ${b.id}`);
});

test('the nodes come in reading order, generation by generation and left to right, for the Tab key', () => {
	const l = layout(rivera, 'en', false, words);
	for (let i = 1; i < l.nodes.length; i++) {
		const [a, b] = [l.nodes[i - 1], l.nodes[i]];
		assert.ok(a.y < b.y || (a.y === b.y && a.x < b.x), `${a.id} before ${b.id}`);
	}
});

test('a single parent drawn: the line to the children starts under their text', () => {
	const one: Pedigree = { ...rivera, nodes: rivera.nodes.filter((n) => n.id !== 'm') };
	const l = layout(one, 'en', false, words);
	const f = l.nodes.find((n) => n.id === 'f')!;
	const drop = l.segs.find((s) => s.x1 === f.x && s.x2 === f.x)!;
	assert.equal(drop.y1, f.y + 36 + (f.lines.length - 1) * 14 + 6);
});

test('names show when asked, and the toggle removes every name from the drawing text', () => {
	const shown = texts(layout(rivera, 'en', false, words)).join('\n');
	assert.ok(shown.includes('Carmen Ortega') && shown.includes('Alex Rivera') && shown.includes('Lucía Rivera'));
	const hidden = texts(layout(rivera, 'es', true, words));
	for (const name of NAMES) assert.ok(!hidden.some((t) => t.includes(name)), `${name} still in ${JSON.stringify(hidden)}`);
	assert.ok(hidden.includes('madre') && hidden.includes('this profile'));
});

test('a relative linked to a profile the viewer cannot read shows no conditions or years', () => {
	const masked: Pedigree = { ...rivera, nodes: [node({ id: 'm', relationship: 'MTH', generation: -1, slot: 35, union: 'parents', masked: true, linked: true })] };
	const m = layout(masked, 'en', false, words).nodes.find((n) => n.id === 'm')!;
	assert.deepEqual(m.lines, ['mth', 'records not visible']);
	assert.equal(m.affected, false);
});

test('fill from household turns the founder household into relatives once', () => {
	const rows = fromHousehold(
		[
			{ slug: 'alex', relationship: 'self', sex_at_birth: 'male' },
			{ slug: 'sam', relationship: 'partner', sex_at_birth: 'female' },
			{ slug: 'lucia', relationship: 'child', sex_at_birth: 'female' },
			{ slug: 'carmen', relationship: 'parent', sex_at_birth: 'female' },
			{ slug: 'marta', relationship: 'other', sex_at_birth: null }
		],
		'alex',
		new Set(['sam'])
	);
	assert.deepEqual(rows.map((r) => [r.relationship, r.linked_profile]), [['DAU', 'lucia'], ['MTH', 'carmen']]);
});

test('fill from household: a refused row does not keep the later rows out', async () => {
	const sent: string[] = [];
	const refused = new Error('already linked');
	const failed = await addEach(['sam', 'lucia', 'carmen'], async (slug) => {
		if (slug === 'lucia') throw refused;
		sent.push(slug);
	});
	assert.deepEqual(sent, ['sam', 'carmen']);
	assert.equal(failed, refused);
	assert.equal(await addEach(['sam'], async () => {}), undefined);
});
