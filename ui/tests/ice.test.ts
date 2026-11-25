// The emergency card form (routes/summary/IceCard.svelte): its expiry preview names the date the
// server will set, also on a page left open past 00:00 UTC.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { iceUntil, utcMidnights } from '../src/lib/logic/ice.ts';

const card = readFileSync(new URL('../src/routes/summary/IceCard.svelte', import.meta.url), 'utf8');
const at = (iso: string) => Date.parse(iso);

test('ice: the preview is 00:00 UTC, the chosen days after the UTC date of now', () => {
	assert.equal(iceUntil(1, at('2026-11-24T23:59:00Z')), '2026-11-25T00:00:00.000Z');
	assert.equal(iceUntil(1, at('2026-11-25T00:00:01Z')), '2026-11-26T00:00:00.000Z');
	assert.equal(iceUntil(7, at('2026-12-28T10:00:00Z')), '2027-01-04T00:00:00.000Z');
	assert.equal(iceUntil(365, at('2026-11-24T12:00:00Z')), '2027-11-24T00:00:00.000Z');
	for (const bad of [0, 366, -1, Number.NaN, '']) assert.equal(iceUntil(bad, at('2026-11-24T12:00:00Z')), '');
});

test('ice: the clock calls the page at each 00:00 UTC until it stops', () => {
	let now = at('2026-11-24T23:59:00Z');
	const timers: { f: () => void; ms: number; live: boolean }[] = [];
	const clock = {
		now: () => now,
		set: (f: () => void, ms: number) => timers.push({ f, ms, live: true }) - 1,
		clear: (h: unknown) => void (timers[h as number].live = false)
	};
	let days = 0;
	const stop = utcMidnights(() => days++, clock);
	assert.deepEqual(timers.map((x) => x.ms), [60_000]);
	now = at('2026-11-25T00:00:00Z');
	timers[0].f();
	assert.equal(days, 1);
	// the next one is a whole day later
	assert.deepEqual(timers.map((x) => x.ms), [60_000, 86_400_000]);
	stop();
	assert.equal(timers[1].live, false);
	// a timer already on its way after the stop does nothing
	timers[1].f();
	assert.equal(days, 1);
	assert.equal(timers.length, 2);
});

test('ice: the form reads its preview from the clock, which moves at 00:00 UTC and when the tab comes back', () => {
	assert.match(card, /iceUntil\(days, now\)/);
	assert.match(card, /utcMidnights\(/);
	assert.match(card, /addEventListener\('visibilitychange'/);
	assert.match(card, /removeEventListener\('visibilitychange'/);
	assert.doesNotMatch(card, /const now = new Date\(\)/);
});
