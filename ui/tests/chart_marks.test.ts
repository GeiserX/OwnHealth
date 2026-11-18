// The biomarker chart's lab and method change labels, and its lab legend (rule pack 0.2.0 makes a lab or
// method switch at the latest point the case people read).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { labsOf, layoutMarks } from '../src/lib/logic/axis.ts';

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
