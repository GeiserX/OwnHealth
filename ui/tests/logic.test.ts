// Unit tests for the pure UI helpers: `pnpm test` (node --test, no extra dependencies).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { axisTicks, buildBars } from '../src/lib/logic/axis.ts';
import { searchTarget } from '../src/lib/logic/search.ts';
import { filenameFromDisposition } from '../src/lib/logic/download.ts';
import { roleCell } from '../src/lib/logic/grants.ts';
import { pluralKey } from '../src/lib/logic/plural.ts';

const covers = (r: { lo: number; hi: number }, nums: number[]) => r.lo <= Math.min(...nums) && r.hi >= Math.max(...nums);

test('bar axes from 0 keep about four steps', () => {
	const steps = [8000, 9200, 10500, 8700];
	const r = axisTicks(steps, 0);
	assert.ok(r.ticks.length <= 6, `steps: ${r.ticks.join(',')}`);
	assert.ok(covers(r, steps));
	assert.equal(r.ticks[0], 0);
});

test('sleep axis from floor(min)-2 keeps about four steps', () => {
	const sleep = [6.6, 7.2, 6.9];
	const r = axisTicks(sleep, Math.max(0, Math.floor(Math.min(...sleep)) - 2));
	assert.ok(r.ticks.length <= 6, `sleep: ${r.ticks.join(',')}`);
	assert.ok(covers(r, sleep));
	assert.equal(r.lo, 4);
});

test('line axes stay padded around the data', () => {
	const hrv = [40, 52, 60];
	const r = axisTicks(hrv);
	assert.ok(r.ticks.length <= 7);
	assert.ok(r.lo < 40 && r.hi > 60);
});

test('an ongoing medication draws as a bar to today, a one-day annotation as a dot', () => {
	const x = (d: string) => Number(d.slice(0, 4)) * 10;
	const bars = buildBars(
		[
			{ date: '2022-06-20', end: '2025-12-01', label: 'Atorvastatin 20 mg' },
			{ date: '2026-03-10', end: null, label: 'Atorvastatin 20 mg' },
			{ date: '2024-05-01', end: '2024-05-01', label: 'Flu shot' }
		],
		x,
		'2027-01-01'
	);
	assert.deepEqual(bars.map((b) => b.span), [true, true, false]);
	assert.equal(bars[1].x2, x('2027-01-01'));
	assert.equal(bars[1].ongoing, true);
	assert.equal(bars[0].ongoing, false);
	// The old course ended before the restart: one row is enough.
	assert.deepEqual([bars[0].row, bars[1].row], [0, 0]);
});

test('two current medications that overlap go on separate rows', () => {
	const x = (d: string) => new Date(d).getTime() / 864e5;
	const bars = buildBars(
		[
			{ date: '2026-03-10', end: null, label: 'Atorvastatin 20 mg' },
			{ date: '2026-04-01', end: null, label: 'Omeprazole 20 mg' }
		],
		x,
		'2026-11-08'
	);
	assert.notEqual(bars[0].row, bars[1].row);
});

test('search passes the typed term on', () => {
	assert.equal(searchTarget('clopidogrel'), '/drug-check?drug=clopidogrel');
	assert.equal(searchTarget('LDL'), '/biomarkers?q=LDL');
	assert.equal(searchTarget('labnorte'), '/documents?q=labnorte');
	assert.equal(searchTarget('  '), null);
	assert.equal(searchTarget('APOE'), '/genome');
});

test('download file name comes from Content-Disposition', () => {
	assert.equal(filenameFromDisposition('attachment; filename="ownhealth-alex-2026-11-08.zip"', 'x.zip'), 'ownhealth-alex-2026-11-08.zip');
	assert.equal(filenameFromDisposition("attachment; filename*=utf-8''ownhealth-luc%C3%ADa.zip", 'x.zip'), 'ownhealth-lucía.zip');
	assert.equal(filenameFromDisposition(null, 'ownhealth-alex.zip'), 'ownhealth-alex.zip');
});

test('grant matrix shows a member as owner of their own profile', () => {
	const grants = [{ user_id: 'alex', profile: 'sam', role: 'viewer' }];
	const sam = { user_id: 'sam', own_profile: 'sam' };
	assert.equal(roleCell(grants, sam, 'sam', false), 'owner');
	assert.equal(roleCell(grants, { user_id: 'alex', own_profile: 'alex' }, 'sam', false), 'viewer');
	assert.equal(roleCell(grants, sam, 'lucia', false), '?');
	assert.equal(roleCell(grants, sam, 'lucia', true), '');
});

test('plural keys pick the singular form for one', () => {
	const has = (k: string) => k === 'n_days_one';
	assert.equal(pluralKey('en', 1, 'n_days', has), 'n_days_one');
	assert.equal(pluralKey('es', 1, 'n_days', has), 'n_days_one');
	assert.equal(pluralKey('en', 2, 'n_days', has), 'n_days');
	assert.equal(pluralKey('en', 0, 'n_days', has), 'n_days');
});
