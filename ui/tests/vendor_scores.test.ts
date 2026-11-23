// The vendor scores card (slice 27): the vendor's own numbers, one row per provider and category,
// labelled with who computed them and kept out of the measurement charts.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { latestRunner, type LoadState } from '../src/lib/logic/loader.ts';
import { dict as strings } from '../src/lib/i18n/dict.ts';
import {
	DEFAULT_DAYS,
	fmtScoreDay,
	inWindow,
	keepWindow,
	MAX_DAYS,
	measurementDays,
	needsWindowRead,
	providerLabel,
	scoreDecimals,
	scoreRows,
	scoreSpanDays,
	windowChoices,
	windowEntry,
	windowWordKey,
	type VendorScore
} from '../src/routes/wearables/scores.ts';

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
	assert.match(page, /const windows = \$derived\(windowChoices\(Math\.max\(span, scoreSpan \?\? 0\)\)\);/);
	const card = readFileSync(new URL('../src/routes/wearables/VendorScores.svelte', import.meta.url), 'utf8');
	assert.match(card, /vendor-scores`, \{ days: MAX_DAYS \}/);
	assert.match(card, /span = s\.loading \? null : scoreSpanDays\(all, Date\.now\(\)\)/);
	assert.match(card, /scoreRows\(inWindow\(all, days, Date\.now\(\)\)\)/);
});

test('vendor scores: a year picked for the scores never labels 90 days of measurements as a year', () => {
	// the page asks the wearables summary for its default 90 days of each series
	assert.equal(measurementDays(365, 90), 90);
	assert.equal(measurementDays(30, 90), 30);
	assert.equal(measurementDays(30, 12), 12);
	assert.equal(measurementDays(7, 0), 7);
	const page = readFileSync(new URL('../src/routes/wearables/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /const mDays = \$derived\(measurementDays\(days, span\)\);/);
	// the charts, their averages, their labels and the subtitle use the measurement days; the card gets the window
	assert.match(page, /m\.series\.slice\(-mDays\)/);
	assert.match(page, /avg: mDays === 30 && m\.avg_30d != null/);
	assert.match(page, /w: windowWord\(mDays\)/);
	assert.match(page, /n: mDays \}\)\}/);
	assert.match(page, /tf\('h1_w_sub', \{ name: profile\?\.display_name \?\? '', d: mDays \}\)/);
	assert.doesNotMatch(page, /slice\(-days\)|windowWord\(days\)|n: days \}|d: days \}/);
	const dict = readFileSync(new URL('../src/lib/i18n/dict.ts', import.meta.url), 'utf8');
	assert.equal(dict.split('\t\th1_wn: "{n} d",').length, 3, 'h1_wn once in en and once in es');
});

test('vendor scores: a window is named by its own days, so 90 days of measurements never read "1 y"', () => {
	const word = (lang: 'en' | 'es', d: number) => {
		const [key, n] = windowWordKey(d);
		return (strings[lang] as Record<string, string>)[key].replace('{n}', String(n));
	};
	assert.deepEqual([7, 30, 90, 365, 12].map((d) => word('en', d)), ['7 d', '30 d', '90 d', '1 y', '12 d']);
	assert.deepEqual([7, 30, 90, 365, 12].map((d) => word('es', d)), ['7 d', '30 d', '90 d', '1 a', '12 d']);
	// the page names every window, the buttons included, through windowWordKey and maps none itself
	const page = readFileSync(new URL('../src/routes/wearables/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /const windowWord = \(d: number\) => \{\s*const \[key, n\] = windowWordKey\(d\);\s*return tf\(key, \{ n \}\);\s*\};/);
	assert.match(page, /\{windowWord\(d\)\}<\/button>/);
	assert.doesNotMatch(page, /h1_w7|h1_w30|h1_w1y|h1_wn/, 'no window word is mapped on the page');
});

test('vendor scores: a profile switch keeps the chosen window while it can, and presses the longest one left when it cannot', () => {
	// While the two reads of the new profile run, the window list is short (7 days only) and not final.
	const switchTo = (days: number, windows: number[]) => [[[7], false], [[7], false], [windows, true]].reduce((d, [w, ready]) => keepWindow(d, w as number[], ready as boolean), days);
	// a year picked on a profile with a year of scores, then a profile with 90 days: 30 days, pressed
	assert.equal(switchTo(365, [7, 30]), 30);
	// and back: the 30 days stay on the profile with a year, which offers the year again
	assert.equal(switchTo(30, [7, 30, 365]), 30);
	// a year picked, then another profile with a year of scores: the loading moment never drops the year
	assert.equal(switchTo(365, [7, 30, 365]), 365);
	assert.equal(switchTo(7, [7, 30, 365]), 7);
	// a profile with less than 30 days offers no choice: the page's first window, cut by the charts to the data
	assert.equal(switchTo(365, [7]), DEFAULT_DAYS);
	assert.equal(switchTo(7, [7]), DEFAULT_DAYS);
	assert.equal(DEFAULT_DAYS, 30);
	// the page keeps `days` through keepWindow once its summary is read and the card has its span, and the
	// card gives no span (null) while its read runs
	const page = readFileSync(new URL('../src/routes/wearables/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /let days = \$state\(DEFAULT_DAYS\);/);
	assert.match(page, /days = keepWindow\(days, windows, !w\.loading && scoreSpan != null\);/);
	assert.match(page, /windowChoices\(Math\.max\(span, scoreSpan \?\? 0\)\)/);
	const card = readFileSync(new URL('../src/routes/wearables/VendorScores.svelte', import.meta.url), 'utf8');
	assert.match(card, /span = s\.loading \? null : scoreSpanDays\(all, Date\.now\(\)\);/);
});

test('an HRV chart of fewer days than the summary read draws the read of those days', () => {
	const days = (n: number) => Array.from({ length: n }, () => ({ value: null }));
	const note = 'HRV from one source only: Apple Watch. Not mixed with HRV from other sources in this window: Oura Ring.';
	const long = { metric: 'hrv_rmssd', series: days(90), limitations: [note], source: 'Apple Watch' };
	// only an HRV entry that left a source out, and only for fewer days than it holds
	assert.equal(needsWindowRead(long, 7), true);
	assert.equal(needsWindowRead(long, 30), true);
	assert.equal(needsWindowRead(long, 90), false);
	assert.equal(needsWindowRead({ ...long, limitations: undefined }, 7), false);
	assert.equal(needsWindowRead({ ...long, metric: 'hr_resting' }, 7), false);
	// the read of the 7 days chose the ring: its entry is drawn, with no note
	const ring = { metric: 'hrv_rmssd', series: days(7), source: 'Oura Ring' };
	const read = { data: [ring], error: null, loading: false };
	assert.equal(windowEntry(long, 7, read).data, ring);
	assert.equal(windowEntry(long, 90, read).data, long);
	const steps = { metric: 'steps', series: days(90) };
	assert.equal(windowEntry(steps, 7, { ...read, data: [{ metric: 'steps', series: days(7) }] }).data, steps);
});

test('a failed HRV window read exposes its error on that card and can be retried', async () => {
	const long = { metric: 'hrv_rmssd', series: Array(30).fill(55), limitations: ['Another source was left out.'] };
	const band = { metric: 'hrv_rmssd', series: Array(7).fill(71) };
	const rest = { metric: 'hr_resting', series: Array(30).fill(60) };
	const window: LoadState<(typeof band)[]> = { data: null, error: null, loading: false };
	const run = latestRunner(window);
	assert.deepEqual(windowEntry(long, 7, null), { data: null, error: null, loading: true });
	const error = new Error('The window read failed');
	await run(async () => { throw error; }, 'lucia:7:en');
	assert.deepEqual(windowEntry(long, 7, window), { data: null, error, loading: false });
	assert.deepEqual(windowEntry(rest, 7, window), { data: rest, error: null, loading: false });

	const retry = run(async () => [band], 'lucia:7:en');
	assert.deepEqual(windowEntry(long, 7, window), { data: null, error: null, loading: true });
	await retry;
	assert.deepEqual(windowEntry(long, 7, window), { data: band, error: null, loading: false });

	const refresh = run(async () => [band], 'lucia:7:en');
	assert.deepEqual(windowEntry(long, 7, window), { data: null, error: null, loading: true });
	await refresh;
});

test('a successful window read with no HRV clears the old series and source', () => {
	const long = {
		metric: 'hrv_rmssd',
		series: Array(30).fill(55),
		source: 'Watch',
		limitations: ['Another source was left out.']
	};
	const window = { data: [], error: null, loading: false };
	assert.deepEqual(windowEntry(long, 7, window), { data: null, error: null, loading: false });
});
