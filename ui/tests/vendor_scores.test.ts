// The vendor scores card (slice 27): the vendor's own numbers, one row per provider and category,
// labelled with who computed them and kept out of the measurement charts.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { providerLabel, scoreDecimals, scoreRows, type VendorScore } from '../src/routes/wearables/scores.ts';

const s = (provider: string, category: string, t: string, value: number, qualifier: string | null = null): VendorScore => ({
	provider,
	category,
	t,
	value,
	qualifier,
	components: null,
	computed_by: 'provider'
});

test('vendor scores: one row per provider and category, in the card order, newest value last', () => {
	const rows = scoreRows([
		s('garmin', 'body_battery', '2026-11-18T07:00:00Z', 62),
		s('oura', 'readiness', '2026-11-18T06:30:00Z', 84, 'Optimal'),
		s('oura', 'readiness', '2026-11-17T06:30:00Z', 81, 'Good'),
		s('whoop', 'recovery', '2026-11-18T06:00:00Z', 55),
		s('garmin', 'readiness', '2026-11-18T06:00:00Z', 70),
		s('oura', 'mystery', '2026-11-18T06:00:00Z', 3)
	]);
	assert.deepEqual(
		rows.map((r) => [r.category, r.provider, r.latest.value, r.values]),
		[
			['readiness', 'garmin', 70, [70]],
			['readiness', 'oura', 84, [81, 84]],
			['recovery', 'whoop', 55, [55]],
			['body_battery', 'garmin', 62, [62]],
			['mystery', 'oura', 3, [3]]
		]
	);
	assert.equal(rows[1].latest.qualifier, 'Optimal');
	assert.deepEqual(scoreRows([]), []);
});

test('vendor scores: brand names stay as the vendor writes them, values as it sends them', () => {
	assert.equal(providerLabel('oura'), 'Oura');
	assert.equal(providerLabel('whoop'), 'WHOOP');
	assert.equal(providerLabel('ultra_human'), 'Ultra Human');
	assert.equal(scoreDecimals(84), 0);
	assert.equal(scoreDecimals(13.4), 1);
});

test('vendor scores: the Wearables page shows the card apart, and every card word exists in es and en', () => {
	const page = readFileSync(new URL('../src/routes/wearables/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /<VendorScores slug=\{profile\?\.slug\} \{days\} \/>/);
	const dict = readFileSync(new URL('../src/lib/i18n/dict.ts', import.meta.url), 'utf8');
	const keys = ['vs_title', 'vs_by_vendor', 'vs_by', 'vs_word', 'vs_note', 'i2_rt_vendor_score'].concat(
		['readiness', 'recovery', 'sleep', 'activity', 'stress', 'strain', 'body_battery'].map((c) => 'vs_c_' + c)
	);
	for (const k of keys) assert.equal(dict.split(`\t\t${k}: `).length, 3, `${k} once in en and once in es`);
});
