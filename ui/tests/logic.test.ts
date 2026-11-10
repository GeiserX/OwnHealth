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
	assert.equal(searchTarget('APOE'), '/genome?q=APOE');
});

test('search sends marker names to Biomarkers and gene symbols to Genome with the term', () => {
	for (const m of ['Lp(a)', 'lpa', 'PCR', 'CRP', 'ALT', 'AST', 'GGT', 'hemoglobina', 'plaquetas', 'platelets', 'leucocitos', 'VCM', 'MCV', 'B12', 'T4', 'eGFR', 'filtrado glomerular', 'albúmina', 'albumin'])
		assert.equal(searchTarget(m), `/biomarkers?q=${encodeURIComponent(m)}`, m);
	assert.equal(searchTarget('CYP2C19'), '/genome?q=CYP2C19');
	// Short marker names match whole words only.
	assert.equal(searchTarget('salt'), '/documents?q=salt');
	assert.equal(searchTarget('pasta'), '/documents?q=pasta');
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

// ---- review fixes (CodeRabbit on the v0.1 ui slice) ----
import { latestRunner, type LoadState } from '../src/lib/logic/loader.ts';
import { poller } from '../src/lib/logic/poll.ts';
import { isTypingTarget } from '../src/lib/logic/keys.ts';
import { hoursMinutes } from '../src/lib/logic/duration.ts';

const deferred = <T>() => {
	let resolve!: (v: T) => void;
	let reject!: (e: unknown) => void;
	const promise = new Promise<T>((a, b) => ((resolve = a), (reject = b)));
	return { promise, resolve, reject };
};

test('loader clears the previous profile data as soon as the key changes', async () => {
	const s: LoadState<string> = { data: null, error: null, loading: false };
	const run = latestRunner(s);
	await run(async () => 'ana-data', 'ana');
	assert.equal(s.data, 'ana-data');
	const next = deferred<string>();
	const p = run(() => next.promise, 'ben');
	assert.equal(s.data, null, 'old profile data still on screen while the new one loads');
	assert.equal(s.loading, true);
	next.resolve('ben-data');
	await p;
	assert.equal(s.data, 'ben-data');
});

test('loader keeps data on a reload with the same key or no key', async () => {
	const s: LoadState<string> = { data: null, error: null, loading: false };
	const run = latestRunner(s);
	await run(async () => 'v1', 'ana');
	const again = deferred<string>();
	const p = run(() => again.promise, 'ana');
	assert.equal(s.data, 'v1');
	again.resolve('v2');
	await p;
	const plain = deferred<string>();
	const q = run(() => plain.promise);
	assert.equal(s.data, 'v2');
	plain.resolve('v3');
	await q;
	assert.equal(s.data, 'v3');
});

test('loader drops an answer that arrives after a newer call', async () => {
	const s: LoadState<string> = { data: null, error: null, loading: false };
	const run = latestRunner(s);
	const a = deferred<string>();
	const pa = run(() => a.promise, 'ana');
	const pb = run(async () => 'ben-data', 'ben');
	await pb;
	a.resolve('ana-data');
	await pa;
	assert.equal(s.data, 'ben-data');
	assert.equal(s.loading, false);
});

const fakeTimers = () => {
	const q: (() => void)[] = [];
	return {
		q,
		timers: { set: (f: () => void) => (q.push(f), q.length - 1), clear: (h: unknown) => void (q[h as number] = () => {}) },
		// Runs scheduled steps; a loop that never ends stops at 50 steps so the test fails instead of hanging.
		async flush() {
			for (let i = 0; q.length && i < 50; i++) {
				q.shift()!();
				await new Promise((r) => setImmediate(r));
			}
		}
	};
};
const settle = () => new Promise((r) => setImmediate(r));

test('poller stops when stopped (unmount), even with an answer in flight', async () => {
	const ft = fakeTimers();
	let ticks = 0;
	let done = 0;
	const inflight = deferred<boolean>();
	const p = poller({ tick: () => (ticks++, inflight.promise), ondone: () => done++, onfail: () => {}, timers: ft.timers });
	p.start();
	p.stop();
	inflight.resolve(true);
	await settle();
	assert.equal(done, 0, 'ondone after stop');
	assert.equal(ft.q.length, 0, 'a timer was scheduled after stop');
	assert.equal(ticks, 1);
});

test('poller: a new start ends the previous loop, so ondone fires once', async () => {
	const ft = fakeTimers();
	let done = 0;
	let n = 0;
	const p = poller({ tick: async () => ++n >= 4, ondone: () => done++, onfail: () => {}, timers: ft.timers });
	p.start();
	await settle();
	p.start();
	await settle();
	await ft.flush();
	assert.equal(done, 1);
});

test('poller gives up after repeated failures instead of polling forever', async () => {
	const ft = fakeTimers();
	let ticks = 0;
	let failed: unknown = null;
	const p = poller({ tick: async () => { ticks++; throw new Error('HTTP 401'); }, ondone: () => {}, onfail: (e) => (failed = e), maxFails: 3, timers: ft.timers });
	p.start();
	await settle();
	await ft.flush();
	assert.equal(ticks, 3);
	assert.ok(failed instanceof Error);
});

test('the / shortcut leaves every kind of text field alone', () => {
	for (const tagName of ['INPUT', 'TEXTAREA', 'SELECT', 'textarea']) assert.equal(isTypingTarget({ tagName }), true, tagName);
	assert.equal(isTypingTarget({ tagName: 'DIV', isContentEditable: true }), true);
	assert.equal(isTypingTarget({ tagName: 'BODY', isContentEditable: false }), false);
	assert.equal(isTypingTarget({ tagName: 'BUTTON' }), false);
	assert.equal(isTypingTarget(null), false);
});

test('hours never read 60 min', () => {
	assert.equal(hoursMinutes(479.7 / 60), '8 h 0 min');
	assert.equal(hoursMinutes(7.9999), '8 h 0 min');
	assert.equal(hoursMinutes(7.5), '7 h 30 min');
	assert.equal(hoursMinutes(6.6), '6 h 36 min');
	assert.equal(hoursMinutes(0), '0 h 0 min');
});

import { errKey, stripErrorName, validationDetail } from '../src/lib/logic/errors.ts';
import { unitLabel } from '../src/lib/logic/format.ts';

const KEYS = new Set([
	'err_insufficient_scope', 'err_recovery', 'err_login', 'err_stepup_pw', 'err_refused', 'err_dns', 'err_tls',
	'err_sender_dup', 'err_sender_other', 'err_folder_overlap', 'err_unsupported_file', 'err_decimal', 'err_no_connection', 'err_pdf_password'
]);
const has = (k: string) => KEYS.has(k);

test('errors read in plain words: the code first, then the common cause, else the server message', () => {
	assert.equal(errKey('insufficient_scope', 'Token lacks read:labs', has), 'err_insufficient_scope');
	assert.equal(errKey('login_failed', 'Wrong name or password.', has), 'err_login');
	assert.equal(errKey('login_failed', 'Wrong password.', has), 'err_stepup_pw');
	assert.equal(errKey('recovery_invalid', 'Unknown or used recovery code.', has), 'err_recovery');
	assert.equal(errKey(null, 'ImapError: connect: Connection refused (os error 61)', has), 'err_refused');
	assert.equal(errKey(null, 'ImapError: resolve: failed to lookup address information: nodename nor servname provided, or not known', has), 'err_dns');
	assert.equal(errKey('conflict', 'This sender is already on the list.', has), 'err_sender_dup');
	assert.equal(errKey('conflict', 'This sender is already on the list for another profile. Change it.', has), 'err_sender_other');
	assert.equal(errKey('invalid', 'another folder inbox already reads this folder, a folder inside it or a folder around it', has), 'err_folder_overlap');
	assert.equal(errKey('unsupported_upload', 'notes.txt: not a PDF, image, Apple Health export, FHIR bundle or genome facts file', has), 'err_unsupported_file');
	assert.equal(errKey('invalid', 'not a decimal: "abc"', has), 'err_decimal');
	assert.equal(errKey(null, 'Failed to fetch', has), 'err_no_connection');
	assert.equal(errKey(null, 'PasswordError: the file is encrypted', has), 'err_pdf_password');
	assert.equal(errKey('bad_input', 'A retraction needs a reason.', has), null);
	assert.equal(stripErrorName('RuntimeError: request to 127.0.0.1 failed'), 'request to 127.0.0.1 failed');
	assert.equal(stripErrorName('plain words'), 'plain words');
});

test('a validation answer names the field that failed, never "Unprocessable Entity"', () => {
	const d = [{ type: 'string_too_short', loc: ['body', 'password'], msg: 'String should have at least 12 characters' }];
	assert.deepEqual(validationDetail(d), { code: 'invalid', message: 'password: String should have at least 12 characters' });
	assert.deepEqual(validationDetail([{ loc: ['body'], msg: 'Field required' }]), { code: 'invalid', message: 'Field required' });
});

test('units print as people write them', () => {
	assert.equal(unitLabel('x10^9/L'), '×10⁹/L');
	assert.equal(unitLabel('x10^3/µL'), '×10³/µL');
	assert.equal(unitLabel('10*3/uL'), '×10³/µL');
	assert.equal(unitLabel('mL/min/1.73m2'), 'mL/min/1.73m²');
	assert.equal(unitLabel('mg/dL'), 'mg/dL');
	assert.equal(unitLabel(null), '');
});
