// Share links (slice 52, /links): only an owner or a guardian sees and uses the page, a link always
// carries the summary and never a category its creator lacks, every word the page builds exists in es
// and en, the PIN rule is the server's, and the QR code comes from an unmodified qrcodegen.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import ts from 'typescript';
import { dict } from '../src/lib/i18n/dict.ts';
import { errKey } from '../src/lib/logic/errors.ts';
import { NAV } from '../src/lib/nav.ts';
import { DAYS, PICKS, accessKey, categoriesFor, mayShare, pinProblem, qrPath, revocable, sharesAny, shownCategories, stateOf } from '../src/routes/links/logic.ts';

const tables = dict as unknown as Record<string, Record<string, string>>;
const page = readFileSync(new URL('../src/routes/links/+page.svelte', import.meta.url), 'utf8');
const STATES = ['active', 'paused', 'revoked', 'expired', 'locked_out'] as const;
const REASONS = ['sharing_off', 'person', 'locked', 'access_changed'] as const;
const KINDS = ['manifest', 'file', 'passcode_wrong', 'passcode_missing'] as const;

test('only an owner or a guardian may share, and the menu shows Share links only to them', () => {
	assert.ok(mayShare('owner') && mayShare('guardian'));
	assert.ok(!mayShare('viewer') && !mayShare('caregiver') && !mayShare(null) && !mayShare(undefined));
	// Carmen's caregiver and Sam's viewer never see it; Alex, owner of himself, does
	assert.ok(!sharesAny([{ role: 'viewer' }, { role: 'caregiver' }]));
	assert.ok(sharesAny([{ role: 'viewer' }, { role: 'guardian' }]));
	assert.ok(!sharesAny(null) && !sharesAny([]));
	const links = NAV.filter((n) => n.path === '/links');
	assert.equal(links.length, 1);
	assert.equal(links[0].sharers, true);
	// /share is the inbound share target and stays open to everyone
	assert.ok(NAV.every((n) => n.path === '/links' || !n.sharers));
});

test('a link always carries the summary, and only the picks its creator holds', () => {
	assert.deepEqual(PICKS, ['read:labs', 'read:records']);
	assert.deepEqual(categoriesFor(['read:labs', 'read:records'], ['read:summary', 'read:labs', 'read:records']), ['read:summary', 'read:labs', 'read:records']);
	assert.deepEqual(categoriesFor([], ['read:summary', 'read:labs']), ['read:summary']);
	assert.deepEqual(categoriesFor(['read:labs', 'read:records'], ['read:summary', 'read:records']), ['read:summary', 'read:records']);
	// a category no link carries never reaches the body, even when picked by hand
	assert.deepEqual(categoriesFor(['read:sensitive', 'read:genome'], ['read:sensitive', 'read:genome']), ['read:summary']);
	assert.ok(DAYS.every((d) => d >= 1 && d <= 365) && DAYS.includes(7));
	// the server lists them sorted by name; the page shows the summary first, then the form's order
	assert.deepEqual(shownCategories(['read:records', 'read:summary', 'read:labs']), ['read:summary', 'read:labs', 'read:records']);
	assert.deepEqual(shownCategories(['read:records', 'read:summary']), ['read:summary', 'read:records']);
});

test('the PIN is empty or 4 to 64 characters, counted as the server counts them', () => {
	assert.equal(pinProblem(''), null);
	assert.equal(pinProblem('123'), 'shl_pin_len');
	assert.equal(pinProblem('1234'), null);
	assert.equal(pinProblem('x'.repeat(64)), null);
	assert.equal(pinProblem('x'.repeat(65)), 'shl_pin_len');
	// four characters outside the BMP are four characters (Rust chars().count()), not eight
	assert.equal(pinProblem('🔒🔒🔒🔒'), null);
	assert.equal(pinProblem('🔒🔒🔒'), 'shl_pin_len');
});

test('every state, reason and access event has words in es and en, and only a live link can be revoked', () => {
	for (const s of STATES) {
		const st = stateOf({ state: s, reason: null });
		for (const lang of ['en', 'es']) assert.ok(tables[lang][st.key], `${lang} ${st.key}`);
		assert.equal(revocable({ state: s }), s === 'active' || s === 'paused', s);
	}
	assert.equal(stateOf({ state: 'active', reason: null }).tone, 'p-ok');
	assert.equal(stateOf({ state: 'paused', reason: 'sharing_off' }).tone, 'p-watch');
	assert.equal(stateOf({ state: 'revoked', reason: 'person' }).tone, 'p-out');
	for (const r of REASONS) {
		const why = stateOf({ state: 'revoked', reason: r }).why ?? '';
		for (const lang of ['en', 'es']) assert.ok(tables[lang][why], `${lang} ${why}`);
	}
	for (const k of KINDS) for (const lang of ['en', 'es']) assert.ok(tables[lang][accessKey(k)], `${lang} ${k}`);
});

test('every key the page names exists in es and en, plural forms included', () => {
	const named = [...page.matchAll(/\bt[fp]?\('([a-z0-9_]+)'\s*[,)]/g)].map((m) => m[1]);
	assert.ok(named.length > 30, 'the scan finds the page keys');
	const built = [...PICKS, 'read:summary'].flatMap((c) => ['shl_c_', 'shl_s_'].map((k) => k + c.replace('read:', '')));
	const plural = ['shl_days_n', 'shl_left_out', 'shl_tries', 'shl_opened'].map((k) => k + '_one');
	for (const lang of ['en', 'es']) {
		const missing = [...named, ...built, ...plural].filter((k) => !tables[lang][k]);
		assert.deepEqual(missing, [], `${lang} is missing ${missing.join(', ')}`);
	}
	const shl = (lang: string) => Object.keys(tables[lang]).filter((k) => k.startsWith('shl_')).sort();
	assert.deepEqual(shl('es'), shl('en'));
});

test('every refusal code of the share routes reads as our own sentence', () => {
	const has = (k: string) => k in tables.en && k in tables.es;
	for (const code of ['not_sharer', 'category_closed', 'sharing_off', 'share_url_too_long']) {
		assert.equal(errKey(code, 'server words', has), 'err_' + code);
	}
});

test('the page never puts the PIN in a URL or a log, and keeps the link in memory only', () => {
	// no input of the form has a name, so even a native submit could not serialize the PIN or the label
	assert.doesNotMatch(page, /<input[^>]*\bname=/);
	assert.doesNotMatch(page, /<form\b/);
	assert.doesNotMatch(page, /console\.|localStorage|sessionStorage|history\.|location\.(hash|search|href)\s*=/);
	assert.match(page, /type="password"[^>]*autocomplete="off"/);
});

test('qrPath draws one rectangle per run of dark modules, inside the quiet zone', () => {
	// X . X
	// X X X
	// . . .
	const m = ['X.X', 'XXX', '...'];
	const d = qrPath(3, (x, y) => m[y][x] === 'X', 4);
	assert.equal(d, 'M4,4h1v1h-1zM6,4h1v1h-1zM4,5h3v1h-3z');
	assert.equal(qrPath(2, () => false), '');
});

// The vendored encoder: Nayuki's file at its pinned commit, changed only by `export` on its three
// namespaces. Upstream sha256 of typescript-javascript/qrcodegen.ts at 9a217d44.
const UPSTREAM = '239c01a99206650266402e8fe5dd34739e7d4e7190df215412fa0d3e2365afa2';
const vendored = readFileSync(new URL('../src/routes/links/qrcodegen.ts', import.meta.url), 'utf8');

test('qrcodegen.ts is the upstream file plus `export` on its three namespaces', () => {
	const lines = vendored.split('\n');
	const header = lines.findIndex((l) => !l.startsWith('//'));
	assert.equal(header, 4, 'a four-line provenance header');
	const body = lines.slice(header).join('\n');
	assert.equal((body.match(/^export namespace /gm) ?? []).length, 3);
	const upstream = body.replace(/^export namespace /gm, 'namespace ');
	assert.equal(createHash('sha256').update(upstream).digest('hex'), UPSTREAM);
});

test('a share link encodes into a QR code with its three finder patterns', async () => {
	// node strips types but cannot run a namespace; TypeScript itself turns the file into a module
	const js = ts.transpileModule(vendored, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } }).outputText;
	const { qrcodegen } = await import('data:text/javascript,' + encodeURIComponent(js));
	const link = 'shlink:/' + 'eyJ1cmwiOiJodHRwczovL3NoYXJlLmV4YW1wbGUvYXBpL3NobC94In0'.repeat(7);
	const qr = qrcodegen.QrCode.encodeText(link, qrcodegen.QrCode.Ecc.MEDIUM);
	assert.ok(qr.size >= 21 && qr.size <= 177);
	const dark = (x: number, y: number) => qr.getModule(x, y) as boolean;
	// a finder pattern: dark 7x7 ring, light ring inside, dark 3x3 centre
	const finder = (ox: number, oy: number) => {
		for (let y = 0; y < 7; y++)
			for (let x = 0; x < 7; x++) {
				const ring = x === 0 || y === 0 || x === 6 || y === 6;
				const centre = x >= 2 && x <= 4 && y >= 2 && y <= 4;
				if (dark(ox + x, oy + y) !== (ring || centre)) return false;
			}
		return true;
	};
	assert.ok(finder(0, 0) && finder(qr.size - 7, 0) && finder(0, qr.size - 7));
	assert.ok(!finder(qr.size - 7, qr.size - 7), 'the fourth corner has no finder');
	const d = qrPath(qr.size, dark);
	assert.ok(d.startsWith('M4,4h7v1h-7z'), d.slice(0, 40));
});
