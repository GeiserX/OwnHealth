// Health-data screens (UI polish batch 3): the pure helpers behind the biomarker, drug-check and wearables fixes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { axisTicks, buildBars } from '../src/lib/logic/axis.ts';
import { decimalText, refRange } from '../src/lib/logic/format.ts';
import { isActionable, withheldBy, sliceStats } from '../src/lib/logic/health.ts';

test('a medication course that ended before the chart starts is not drawn as a sliver at the left edge', () => {
	const x = (d: string) => Number(d.slice(0, 4)) * 10;
	const bars = buildBars(
		[
			{ date: '2022-06-20', end: '2025-12-01', label: 'Atorvastatin 20 mg' },
			{ date: '2026-03-10', end: null, label: 'Atorvastatin 20 mg' }
		],
		x,
		'2026-11-10',
		'2026-01-01'
	);
	assert.deepEqual(bars.map((b) => b.label + (b.ongoing ? ' (on)' : '')), ['Atorvastatin 20 mg (on)']);
});

test('a converted unit reads its range from the numbers, in the reader language', () => {
	const f = (v: number) => String(v).replace('.', ',');
	assert.equal(refRange(null, 3.36, f), '< 3,36');
	assert.equal(refRange(0.4, 4, f), '0,4–4');
	assert.equal(refRange(30, null, f), '> 30');
	assert.equal(refRange(null, null, f), '—');
});

test('a typed value is a number with a point or a comma, sent with a point', () => {
	assert.equal(decimalText('5,6'), '5.6');
	assert.equal(decimalText(' 97 '), '97');
	assert.equal(decimalText('-2.5'), '-2.5');
	assert.equal(decimalText('abc'), null);
	assert.equal(decimalText('1.2.3'), null);
	assert.equal(decimalText(''), null);
});

test('a withheld scope is found with or without its read: prefix', () => {
	assert.equal(withheldBy(['genome'], 'genome'), true);
	assert.equal(withheldBy(['read:genome'], 'genome'), true);
	assert.equal(withheldBy(['read:genome_raw'], 'genome'), false);
	assert.equal(withheldBy([], 'labs'), false);
});

test('a normal metabolizer is not painted as a warning', () => {
	assert.equal(isActionable('Normal Metabolizer'), false);
	assert.equal(isActionable('Normal Function'), false);
	assert.equal(isActionable('Intermediate Metabolizer'), true);
	assert.equal(isActionable('Poor Metabolizer'), true);
	assert.equal(isActionable(null), false);
});

test('the 7-day window shows the 7-day average and coverage, not the 30-day ones', () => {
	const s = sliceStats([60, null, 62, null, 64, null, null]);
	assert.equal(s.avg, 62);
	assert.equal(s.days, 3);
	assert.equal(s.coverage, 43);
	assert.equal(sliceStats([null, null]).avg, null);
});

test('an SpO2 axis never passes 100 % and a 1 bpm change is not a cliff', () => {
	const spo2 = axisTicks([97, 98], undefined, { max: 100, minSpan: 10 });
	assert.ok(spo2.hi <= 100, `spo2 hi ${spo2.hi}`);
	assert.ok(spo2.hi - spo2.lo >= 10, `spo2 span ${spo2.lo}-${spo2.hi}`);
	const hr = axisTicks([76, 77.3], undefined, { minSpan: 10 });
	assert.ok(hr.hi - hr.lo >= 10, `hr span ${hr.lo}-${hr.hi}`);
	assert.ok(hr.lo <= 76 && hr.hi >= 77.3);
});

import { browserErrorCode, errKey } from '../src/lib/logic/errors.ts';
test('a refused hand-entered value reads in the reader language', () => {
	const has = (k: string) => ['err_decimal', 'err_future_date'].includes(k);
	assert.equal(errKey('bad_input', 'The value is not a number. Type it as it is printed, for example 5,4.', has), 'err_decimal');
	assert.equal(errKey('bad_input', 'The date is in the future. Use the day the value was measured.', has), 'err_future_date');
});

test('passkeys: the browser\'s WebAuthn refusals get our own words, any other error keeps its message', () => {
	assert.equal(browserErrorCode('SecurityError'), 'wa_security');
	assert.equal(browserErrorCode('NotAllowedError'), 'wa_not_allowed');
	assert.equal(browserErrorCode('InvalidStateError'), 'wa_invalid_state');
	assert.equal(browserErrorCode('AbortError'), 'wa_abort');
	assert.equal(browserErrorCode('TypeError'), null);
});
