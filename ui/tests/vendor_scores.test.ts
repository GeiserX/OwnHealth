// The vendor scores card (slice 27): the vendor's own numbers, one row per provider and category,
// labelled with who computed them and kept out of the measurement charts.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fmtScoreDay, inWindow, MAX_DAYS, providerLabel, scoreDecimals, scoreRows, scoreSpanDays, windowChoices, type VendorScore } from '../src/routes/wearables/scores.ts';

const watch = { id: 'b1c2', label: "Sam's Gadgetbridge" };
const s = (provider: string, category: string, t: string, value: number, qualifier: string | null = null, source: VendorScore['source'] = watch): VendorScore => ({
	provider,
	category,
	t,
	value,
	qualifier,
	components: null,
	computed_by: 'provider',
	source
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
	assert.match(page, /<VendorScores slug=\{profile\?\.slug\} \{days\} bind:span=\{scoreSpan\} \/>/);
	const dict = readFileSync(new URL('../src/lib/i18n/dict.ts', import.meta.url), 'utf8');
	const keys = ['vs_title', 'vs_by_vendor', 'vs_by', 'vs_word', 'vs_via', 'vs_no_source', 'vs_note', 'i2_rt_vendor_score'].concat(
		['readiness', 'recovery', 'sleep', 'activity', 'stress', 'strain', 'body_battery'].map((c) => 'vs_c_' + c)
	);
	for (const k of keys) assert.equal(dict.split(`\t\t${k}: `).length, 3, `${k} once in en and once in es`);
});

test('vendor scores: a score keyed at midnight UTC shows its own day west of UTC, as the charts do', () => {
	const tz = process.env.TZ;
	process.env.TZ = 'America/New_York';
	try {
		assert.equal(fmtScoreDay('2026-11-21T00:00:00Z', 'en'), '21 Nov');
		assert.equal(fmtScoreDay('2026-11-21T00:00:00Z', 'es'), '21 nov');
	} finally {
		process.env.TZ = tz;
	}
});

test("vendor scores: the vendor's own word is attributed on screen, not only in a hover title", () => {
	const card = readFileSync(new URL('../src/routes/wearables/VendorScores.svelte', import.meta.url), 'utf8');
	assert.doesNotMatch(card, /title=\{tf\('vs_word'/, 'a phone cannot show a title');
	assert.match(card, />[^<]*\{tf\('vs_word', \{ p: who \}\)\}/, 'the attribution is text in the card');
	const dict = readFileSync(new URL('../src/lib/i18n/dict.ts', import.meta.url), 'utf8');
	assert.match(dict, /\t\tvs_word: "palabra de \{p\}",/, 'es names the vendor next to its word');
	assert.match(dict, /\t\tvs_word: "\{p\}'s word",/, 'en too');
});

test('vendor scores: two connections that bring the same vendor keep a row each, named by connection', () => {
	const phone = { id: 'a9f0', label: 'Health Auto Export' };
	const rows = scoreRows([
		s('garmin', 'body_battery', '2026-11-20T08:00:00Z', 62, null, watch),
		s('garmin', 'body_battery', '2026-11-20T08:00:00Z', 64, null, phone),
		s('garmin', 'body_battery', '2026-11-21T08:00:00Z', 70, null, phone),
		s('garmin', 'body_battery', '2026-11-19T08:00:00Z', 40, null, null)
	]);
	assert.deepEqual(
		rows.map((r) => [r.source?.label ?? null, r.values]),
		[
			['Health Auto Export', [64, 70]],
			["Sam's Gadgetbridge", [62]],
			[null, [40]]
		]
	);
	const card = readFileSync(new URL('../src/routes/wearables/VendorScores.svelte', import.meta.url), 'utf8');
	assert.match(card, /tf\('vs_via', \{ s: r\.source\.label \}\)/, 'the card names the connection');
});

test('vendor scores: a profile with a year of scores and no long measurement series can pick a year', () => {
	const now = Date.parse('2026-11-21T12:00:00Z');
	const day = (n: number) => new Date(now - n * 86_400_000).toISOString();
	const year = Array.from({ length: 365 }, (_, n) => s('oura', 'readiness', day(n + 0.25), 70 + (n % 20)));
	assert.equal(scoreSpanDays(year, now), 365);
	// no measurement series at all (span 0), or one of the summary's 90 days: the scores still open a year
	assert.deepEqual(windowChoices(Math.max(0, scoreSpanDays(year, now))), [7, 30, 365]);
	assert.deepEqual(windowChoices(Math.max(90, scoreSpanDays(year, now))), [7, 30, 365]);
	// three weeks of scores fill no 30-day window; none at all leave the measurements to decide
	assert.deepEqual(windowChoices(scoreSpanDays(year.slice(0, 21), now)), [7]);
	assert.equal(scoreSpanDays([], now), 0);
	assert.deepEqual(windowChoices(Math.max(40, scoreSpanDays([], now))), [7, 30]);
	// the card shows the chosen window of the year it read, as the route would send it
	assert.equal(inWindow(year, 7, now).length, 7);
	assert.equal(inWindow(year, 30, now).length, 30);
	assert.equal(inWindow(year, 365, now).length, 365);
	assert.equal(MAX_DAYS, 365);
});

test('vendor scores: the page counts the scores\' span in its window list, and the card reads the whole year', () => {
	const page = readFileSync(new URL('../src/routes/wearables/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /const windows = \$derived\(windowChoices\(Math\.max\(span, scoreSpan\)\)\);/);
	const card = readFileSync(new URL('../src/routes/wearables/VendorScores.svelte', import.meta.url), 'utf8');
	assert.match(card, /vendor-scores`, \{ days: MAX_DAYS \}/);
	assert.match(card, /span = scoreSpanDays\(all, Date\.now\(\)\)/);
	assert.match(card, /scoreRows\(inWindow\(all, days, Date\.now\(\)\)\)/);
});
