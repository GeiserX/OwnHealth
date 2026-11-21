// A dense vendor score history (a value every 3 minutes for a year, as intraday Body Battery or stress
// would be): about 200,000 points in one row. Grouping it stays linear, and its sparkline bounds never
// spread the whole series into one call.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { sparkBounds } from '../src/lib/components/spark.ts';
import { scoreRows, type VendorScore } from '../src/routes/wearables/scores.ts';

const N = 200_000;
const start = Date.parse('2025-11-21T00:00:00Z');
const watch = { id: 'b1c2', label: "Sam's Gadgetbridge" };
// shuffled in blocks, so the grouping must sort, and the lowest and highest values sit mid-series
const dense: VendorScore[] = Array.from({ length: N }, (_, k) => {
	const i = (k * 7919) % N;
	return {
		provider: 'garmin',
		category: 'body_battery',
		t: new Date(start + i * 180_000).toISOString(),
		value: i === 1234 ? -3 : i === 98765 ? 104 : 5 + (i % 90),
		qualifier: null,
		components: null,
		computed_by: 'provider',
		source: watch
	};
});

test('vendor scores: 200,000 points of one score group in linear time, oldest first', () => {
	const t0 = performance.now();
	const rows = scoreRows(dense);
	const ms = performance.now() - t0;
	assert.equal(rows.length, 1);
	assert.equal(rows[0].values.length, N);
	assert.equal(rows[0].latest.t, new Date(start + (N - 1) * 180_000).toISOString());
	assert.deepEqual(rows[0].values.slice(0, 3), [5, 6, 7]);
	assert.ok(ms < 2000, `grouping took ${Math.round(ms)} ms`);
});

test('vendor scores: the sparkline bounds of 200,000 values, with and without a band', () => {
	const values = scoreRows(dense)[0].values;
	assert.deepEqual(sparkBounds(values, null), [-3, 104]);
	assert.deepEqual(sparkBounds(values, [-10, 50]), [-10, 104]);
	assert.deepEqual(sparkBounds([3], null), [3, 3]);
});

test('vendor scores: the sparkline takes its bounds from sparkBounds, never from a spread', () => {
	const line = readFileSync(new URL('../src/lib/components/Sparkline.svelte', import.meta.url), 'utf8');
	assert.match(line, /import \{ sparkBounds \} from '\.\/spark';/);
	assert.match(line, /const \[mn, mx\] = sparkBounds\(vals, band\);/);
	assert.doesNotMatch(line, /\.\.\.\s*vals|Math\.(min|max)\(\s*\.\.\./, 'no series is spread into one call');
});
