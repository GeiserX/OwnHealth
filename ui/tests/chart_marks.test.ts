// The biomarker chart's lab and method change labels, and its lab legend (rule pack 0.2.0 makes a lab or
// method switch at the latest point the case people read).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { labsOf, layoutMarks, measKey } from '../src/lib/logic/axis.ts';
import { pluralKey } from '../src/lib/logic/plural.ts';

test('a change label near the right edge is drawn to the left of its line, never cut off', () => {
	const [m] = layoutMarks([{ x: 830, label: 'cambio de laboratorio' }], 840);
	assert.equal(m.anchor, 'end');
	assert.equal(m.tx, 826);
	assert.ok(m.tx - m.label.length * 5.6 >= 0);
	const [left] = layoutMarks([{ x: 200, label: 'cambio de laboratorio' }], 840);
	assert.deepEqual([left.anchor, left.tx, left.row], ['start', 204, 0]);
});

test('two labels that would overlap take separate rows, whichever side they are drawn on', () => {
	const ms = layoutMarks(
		[
			{ x: 700, label: 'lab change' },
			{ x: 760, label: 'method change' }
		],
		800
	);
	assert.deepEqual(
		ms.map((m) => m.row),
		[0, 1]
	);
});

test('a point entered by hand is not counted as a lab', () => {
	const pts = [{ lab: 'LabNorte' }, { lab: 'LabNorte' }, { lab: null }];
	assert.deepEqual(labsOf(pts), { keys: ['LabNorte', null], count: 1 });
	assert.deepEqual(labsOf([{ lab: 'A' }, { lab: 'B' }]), { keys: ['A', 'B'], count: 2 });
});

test('a series with only hand-entered values says how many measurements, never "0 labs"', async () => {
	const { dict } = await import('../src/lib/i18n/dict.ts');
	const d = dict as Record<string, Record<string, string>>;
	const fill = (s: string, v: Record<string, string | number>) => s.replace(/\{(\w+)\}/g, (_, k) => String(v[k]));
	const sub = (lang: 'en' | 'es', pts: { lab?: string | null }[]) => {
		const { count } = labsOf(pts);
		const pk = (key: string, n: number) => d[lang][pluralKey(lang, n, key, (k) => k in d.en)];
		const labs = fill(pk('h1_labs', count), { n: count });
		return fill(pk(measKey(count), pts.length), { n: pts.length, labs, years: '2026' });
	};
	const byHand = [{ lab: null }, { lab: null }];
	assert.equal(sub('es', byHand), '2 mediciones · 2026');
	assert.equal(sub('en', byHand), '2 measurements · 2026');
	assert.equal(sub('es', [{ lab: null }]), '1 medición · 2026');
	assert.equal(sub('en', [{ lab: null }]), '1 measurement · 2026');
	assert.equal(sub('es', [{ lab: 'LabNorte' }, { lab: null }]), '2 mediciones de 1 laboratorio · 2026');
	assert.equal(sub('en', [{ lab: 'LabNorte' }, { lab: 'LabNorte' }]), '2 measurements from 1 lab · 2026');
});
