// Share links (slice 52, /links): only an owner or a guardian sees and uses the page, a link always
// carries the summary and never a category its creator lacks, every word the page builds exists in es
// and en, the PIN rule is the server's, and the QR code comes from an unmodified qrcodegen.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import ts from 'typescript';
import { dict } from '../src/lib/i18n/dict.ts';
import { errKey } from '../src/lib/logic/errors.ts';
import { NAV, navFor } from '../src/lib/nav.ts';
import { DAYS, PICKS, accessKey, canShare, categoriesFor, expiryKey, mayShare, pinProblem, qrPath, revocable, sharesAny, shownCategories, stateOf } from '../src/routes/links/logic.ts';

const tables = dict as unknown as Record<string, Record<string, string>>;
const page = readFileSync(new URL('../src/routes/links/+page.svelte', import.meta.url), 'utf8');
const STATES = ['active', 'paused', 'revoked', 'expired', 'locked_out'] as const;
const REASONS = ['sharing_off', 'person', 'locked', 'access_changed'] as const;
const KINDS = ['manifest', 'file', 'passcode_wrong', 'passcode_missing'] as const;
// an expiry far from every `now` the tests pass
const LATER = '2099-01-01T00:00:00Z';

test('only an owner or a guardian may share, and the menu shows Share links only to them', () => {
	assert.ok(mayShare('owner') && mayShare('guardian'));
	assert.ok(!mayShare('viewer') && !mayShare('caregiver') && !mayShare(null) && !mayShare(undefined));
	// Carmen's caregiver and Sam's viewer never see it; Alex, owner of himself, does
	assert.ok(!sharesAny([{ role: 'viewer' }, { role: 'caregiver' }]));
	assert.ok(sharesAny([{ role: 'viewer' }, { role: 'guardian' }]));
	assert.ok(!sharesAny(null) && !sharesAny([]));
	// a break-glass grant reads and never shares (routes/shl.rs sharer), whatever its role
	assert.ok(!mayShare('guardian', true) && !mayShare('owner', true) && mayShare('guardian', false));
	assert.ok(!sharesAny([{ role: 'guardian', break_glass: true }, { role: 'viewer', break_glass: false }]));
	assert.ok(sharesAny([{ role: 'guardian', break_glass: true }, { role: 'owner', break_glass: false }]));
	// the menu the layout draws: navFor(sharesAny(profiles)), and Share links only for a sharer
	assert.ok(!navFor(false).some((n) => n.path === '/links'));
	assert.ok(navFor(true).some((n) => n.path === '/links'));
	assert.equal(navFor(false).length, NAV.length - 1);
	const layout = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
	assert.match(layout, /\{#each navFor\(sharesAny\(me\?\.profiles\)\) as n \(n\.path\)\}/);
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
	// the server's open set decides; without the summary no link can be made
	assert.ok(canShare(['read:summary']) && !canShare(['read:labs', 'read:records']) && !canShare([]));
	assert.doesNotMatch(page, /prof\?\.scopes|\bheld\b/, 'the form never decides from the /me scopes');
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
	// no maxlength on the PIN: the browser counts UTF-16 units, so 33 to 64 such characters could not be typed
	const input = page.match(/<input[^>]*type="password"[^>]*>/)?.[0] ?? '';
	assert.ok(input, 'the PIN input');
	assert.doesNotMatch(input, /maxlength/);
});

test('a link row says when it was made in words for a link, and a past expiry as past', () => {
	assert.doesNotMatch(page, /i2_added/, 'not the token word (es "añadida")');
	assert.equal(tables.es.shl_created, 'creado el');
	const now = Date.parse('2026-11-22T12:00:00Z');
	assert.equal(expiryKey({ expires_at: '2026-11-29T12:00:00Z' }, now), 'shl_expires');
	assert.equal(expiryKey({ expires_at: '2026-11-22T11:59:59Z' }, now), 'shl_expired_on');
	assert.equal(expiryKey({ expires_at: '2026-11-22T12:00:00Z' }, now), 'shl_expired_on');
	assert.match(page, /t\(expiryKey\(x, now\)\)/, 'every state, a revoked or locked-out link too');
});

test('a link whose expiry passes while the page is open reads expired, without Revoke, on the next minute', () => {
	const now = Date.parse('2026-11-22T12:00:00Z');
	const soon = '2026-11-22T12:00:30Z';
	const past = '2026-11-22T11:59:59Z';
	// before its expiry the server's word stands
	for (const s of STATES) assert.equal(stateOf({ state: s, reason: null, expires_at: soon }, now).key, 'shl_st_' + s, s);
	// after it, a live or locked-out link has expired, as the server would now say (routes/shl.rs state)
	for (const s of ['active', 'paused', 'locked_out'] as const) {
		const x = { state: s, reason: s === 'paused' ? ('sharing_off' as const) : null, expires_at: past };
		assert.deepEqual(stateOf(x, now), { key: 'shl_st_expired', why: null, tone: 'p-out' }, s);
		assert.equal(revocable(x, now), false, s);
	}
	// a revoked link stays revoked, with its reason
	assert.deepEqual(stateOf({ state: 'revoked', reason: 'person', expires_at: past }, now), { key: 'shl_st_revoked', why: 'shl_why_person', tone: 'p-out' });
	assert.equal(revocable({ state: 'active', expires_at: soon }, now), true);
	// one clock for the rows, moved each minute and cleared when the page goes
	assert.match(page, /setInterval\(\(\) => \(now = Date\.now\(\)\), 60_000\)/);
	assert.match(page, /return \(\) => clearInterval\(/);
	assert.match(page, /stateOf\(x, now\)/);
	assert.match(page, /revocable\(x, now\)/);
	assert.doesNotMatch(page, /Date\.now\(\)\)\)/, 'no row reads the time by itself');
});

test('the step-up note is not part of a heading, and the new link has a live region and the focus', () => {
	assert.match(page, /<h3 id="shl-new">\{t\('shl_new'\)\}<\/h3>/);
	// the live region is outside {#if fresh}, so it is in the page before the link is put in it
	const live = page.indexOf('aria-live="polite"');
	assert.ok(live > 0 && live < page.indexOf('{#if fresh}'), 'live region before the fresh block');
	assert.match(page, /<h3 id="shl-made" tabindex="-1" bind:this=\{madeEl\}>/);
	assert.match(page, /madeEl\?\.focus\(\)/);
	// after a revoke the keyboard stays on the row: its access log, or the row itself (an offline card has no log)
	assert.match(page, /\(document\.getElementById\(`shl-log-\$\{x\.id\}`\) \?\? document\.getElementById\(`shl-row-\$\{x\.id\}`\)\)\?\.focus\(\)/);
	assert.match(page, /<li data-testid="shl-row" id="shl-row-\{x\.id\}" tabindex="-1">/);
});

test('every state, reason and access event has words in es and en, and only a live link can be revoked', () => {
	for (const s of STATES) {
		const st = stateOf({ state: s, reason: null, expires_at: LATER }, 0);
		for (const lang of ['en', 'es']) assert.ok(tables[lang][st.key], `${lang} ${st.key}`);
		assert.equal(revocable({ state: s, expires_at: LATER }, 0), s === 'active' || s === 'paused', s);
	}
	assert.equal(stateOf({ state: 'active', reason: null, expires_at: LATER }, 0).tone, 'p-ok');
	assert.equal(stateOf({ state: 'paused', reason: 'sharing_off', expires_at: LATER }, 0).tone, 'p-watch');
	assert.equal(stateOf({ state: 'revoked', reason: 'person', expires_at: LATER }, 0).tone, 'p-out');
	for (const r of REASONS) {
		const why = stateOf({ state: 'revoked', reason: r, expires_at: LATER }, 0).why ?? '';
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
	for (const code of ['not_sharer', 'category_closed', 'summary_closed', 'sharing_off', 'share_url_too_long']) {
		assert.equal(errKey(code, 'server words', has), 'err_' + code);
	}
});

test('the page never puts the PIN in a URL or a log, and keeps the link in memory only', () => {
	// no input of the form has a name, so even a native submit could not serialize the PIN or the label
	assert.doesNotMatch(page, /<input[^>]*\bname=/);
	assert.doesNotMatch(page, /<form\b/);
	assert.match(page, /type="password"[^>]*autocomplete="off"/);
	// every file of the page but the vendored encoder: no log, no storage, no history, no URL write
	const dir = new URL('../src/routes/links/', import.meta.url);
	const files = readdirSync(dir).filter((f) => f !== 'qrcodegen.ts');
	assert.ok(files.includes('+page.svelte') && files.includes('api.ts') && files.includes('logic.ts') && files.includes('qr.ts'), files.join(' '));
	for (const f of files) {
		const src = readFileSync(new URL(f, dir), 'utf8');
		assert.doesNotMatch(src, /console\.|localStorage|sessionStorage|history\.|location\.(hash|search|href)\s*=/, f);
	}
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

/** The file as upstream ships it, whatever line endings the checkout gave it (git may write CRLF). */
function upstreamOf(text: string): { header: number; exports: number; sha: string } {
	const lines = text.replace(/\r\n/g, '\n').split('\n');
	const header = lines.findIndex((l) => !l.startsWith('//'));
	const body = lines.slice(header).join('\n');
	const upstream = body.replace(/^export namespace /gm, 'namespace ');
	return { header, exports: (body.match(/^export namespace /gm) ?? []).length, sha: createHash('sha256').update(upstream).digest('hex') };
}

test('qrcodegen.ts is the upstream file plus `export` on its three namespaces', () => {
	const got = upstreamOf(vendored);
	assert.equal(got.header, 4, 'a four-line provenance header');
	assert.equal(got.exports, 3);
	assert.equal(got.sha, UPSTREAM);
	// a CRLF checkout of the same file passes; a changed encoder does not
	assert.equal(upstreamOf(vendored.replace(/\r?\n/g, '\r\n')).sha, UPSTREAM, 'CRLF checkout');
	assert.notEqual(upstreamOf(vendored.replace('Ecc.MEDIUM', 'Ecc.LOW')).sha, UPSTREAM, 'the control: an edit');
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

/** A module of src/routes/links/ as TypeScript turns it into JavaScript, its imports replaced. */
function moduleUrl(file: string, imports: Record<string, string> = {}): string {
	let src = readFileSync(new URL('../src/routes/links/' + file, import.meta.url), 'utf8');
	src = ts.transpileModule(src, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } }).outputText;
	for (const [from, to] of Object.entries(imports)) src = src.replaceAll(`from '${from}'`, () => `from '${to}'`);
	return 'data:text/javascript;base64,' + Buffer.from(src).toString('base64');
}

test('the page draws its QR code with error correction M, as the spec says', async () => {
	const logic = moduleUrl('logic.ts');
	const enc = moduleUrl('qrcodegen.ts');
	const { qrOf, QUIET } = await import(moduleUrl('qr.ts', { './logic': logic, './qrcodegen': enc }));
	const { qrcodegen } = await import(enc);
	const link = 'shlink:/' + 'eyJ1cmwiOiJodHRwczovL3NoYXJlLmV4YW1wbGUvYXBpL3NobC94In0'.repeat(4);
	const drawn = (ecc: unknown) => {
		const qr = qrcodegen.QrCode.encodeText(link, ecc);
		return { size: qr.size + 2 * QUIET, path: qrPath(qr.size, (x: number, y: number) => qr.getModule(x, y), QUIET) };
	};
	const { LOW, MEDIUM, QUARTILE } = qrcodegen.QrCode.Ecc;
	// a control that can fail: the levels draw different codes for this link
	assert.notDeepEqual(drawn(LOW), drawn(MEDIUM));
	assert.notDeepEqual(drawn(QUARTILE), drawn(MEDIUM));
	assert.deepEqual(qrOf(link), drawn(MEDIUM));
});
