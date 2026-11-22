// Share links (slice 52): a link made on /links opens in a reference SMART Health Links viewer
// (shc-web-reader, pinned in shl-viewer.sh, served on its own origin), with its PIN when it has one.
// After revoke, and after its expiry, the same link gives 404 and the viewer says so. The red control:
// the same link with a wrong key shows the viewer's error, never the summary. The viewer is the pinned
// commit as it is, unpatched. The PIN, the link and its key never reach a URL, a console, the
// history or the storage of the OwnHealth page, and the QR code decodes to the link. The form offers
// only what the server accepts (locks, break-glass), and a link made just before a profile switch
// never shows as the next profile's. Requests to anything but this machine are refused, so no step
// depends on the internet.
import { execFileSync } from 'node:child_process';
import { expect, test, type BrowserContext, type Page } from '@playwright/test';

const VIEWER = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_VIEWER_PORT}`;
const DB = process.env.OWNHEALTH_E2E_DB ?? '';
// the name the viewer gives itself in the manifest request (src/lib/SHX.js SHL_RECIPIENT)
const RECIPIENT = 'SMART Health Card Web Reader';

/** A fresh secret for this run: no PIN is written in the repository. */
const secret = () => `pin-${Math.random().toString(36).slice(2, 10)}`;

/** What the browser did, to prove no secret reaches it: every request URL of the context, the
 * console lines and the main-frame URLs (history entries) of the OwnHealth page, and the console of
 * every page opened later (the viewer). */
interface Seen {
	urls: string[];
	appLogs: string[];
	appUrls: string[];
	viewerLogs: string[];
}

/** Hooked on the fixture page itself before it navigates: context.on('page') never sees that page. */
function watch(context: BrowserContext, page: Page): Seen {
	const seen: Seen = { urls: [], appLogs: [], appUrls: [], viewerLogs: [] };
	context.on('request', (r) => seen.urls.push(r.url()));
	page.on('console', (m) => seen.appLogs.push(m.text()));
	page.on('framenavigated', (f) => {
		if (f === page.mainFrame()) seen.appUrls.push(f.url());
	});
	context.on('page', (p) => p.on('console', (m) => seen.viewerLogs.push(m.text())));
	return seen;
}
let seen: Seen;

/** The pieces of a link that must never leave the page: its PIN, its key, the link itself (its
 * base64url body: the key's own characters never appear in it) and the manifest id. */
function secretsOf(link: string, pin?: string): { all: string[]; key: string; id: string } {
	const p = payload(link);
	const id = p.url.split('/').pop() ?? '';
	const all = [p.key, link.slice(8, 48), link.slice(-40), id, ...(pin ? [pin] : [])];
	return { all, key: p.key, id };
}

/** The requests that carry a link's pieces by design: the viewer navigations the test makes (the
 * viewer's own page, the link after '#'), and the viewer's manifest POST and file GETs to the app's
 * own SHL endpoint, on its exact origin and path. A request anywhere else is a leak. */
function exempt(u: string, manifest: string): boolean {
	if (u.startsWith(`${VIEWER}/#`)) return true;
	const url = new URL(u);
	const m = new URL(manifest);
	if (url.origin !== m.origin || url.search || url.hash) return false;
	const file = `${m.pathname}/file/`;
	return url.pathname === m.pathname || (url.pathname.startsWith(file) && /^[A-Za-z0-9_-]{43}$/.test(url.pathname.slice(file.length)));
}

/** No secret of the link in a request URL (but the exempt ones above), nor in the OwnHealth page's
 * console, history or storage once the person left the page; the viewer's console holds neither the
 * PIN nor the key. */
async function assertNoLeak(page: Page, link: string, pin?: string) {
	const { all, key, id } = secretsOf(link, pin);
	const manifest = payload(link).url;
	const has = (s: string, list: string[]) => list.some((x) => x && s.includes(x));
	const leaked = (urls: string[]) => urls.filter((u) => !exempt(u, manifest) && has(u, all));
	// the control: the manifest id sent to another origin is a leak
	expect(leaked([`https://collector.invalid/api/shl/${id}`])).toHaveLength(1);
	expect(leaked(seen.urls)).toEqual([]);
	expect(seen.urls.filter((u) => has(u, [key, ...(pin ? [pin] : [])]))).toEqual([]);
	expect(seen.appLogs.filter((l) => has(l, all))).toEqual([]);
	expect(seen.viewerLogs.filter((l) => has(l, [key, ...(pin ? [pin] : [])]))).toEqual([]);
	expect(seen.appUrls.filter((u) => has(u, all))).toEqual([]);
	// leave the page, then read what the browser kept for the app
	await page.goto('/summary?lang=en');
	const kept = await page.evaluate(() => {
		const out: string[] = [];
		for (const st of [localStorage, sessionStorage]) for (let i = 0; i < st.length; i++) out.push(`${st.key(i)}=${st.getItem(st.key(i) ?? '')}`);
		return out;
	});
	expect(kept.length, 'the control: the app does store its profile').toBeGreaterThan(0);
	expect(kept.filter((v) => has(v, all))).toEqual([]);
	expect(seen.appUrls.filter((u) => has(u, all))).toEqual([]);
}

async function offline(context: BrowserContext) {
	await context.route(
		(url) => url.hostname !== '127.0.0.1',
		(route) => route.abort()
	);
}

function payload(shlink: string): { url: string; key: string; exp: number; flag?: string; label?: string } {
	const b64 = shlink.replace(/^shlink:\//, '').replace(/-/g, '+').replace(/_/g, '/');
	return JSON.parse(atob(b64));
}

function withKey(shlink: string, key: string): string {
	const p = { ...payload(shlink), key };
	return 'shlink:/' + btoa(JSON.stringify(p)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/** Fills the new-link form of the profile on screen and returns the link it shows once. */
async function makeLink(page: Page, opts: { label: string; pin?: string; make: string; ready: RegExp }): Promise<string> {
	await page.getByLabel(/^(Label|Etiqueta)/).fill(opts.label);
	if (opts.pin) await page.getByLabel(/^PIN/).fill(opts.pin);
	await page.getByRole('button', { name: opts.make, exact: true }).click();
	await expect(page.getByRole('heading', { name: opts.ready })).toBeVisible();
	const link = (await page.getByTestId('shl-link').textContent())?.trim() ?? '';
	expect(link).toMatch(/^shlink:\/[A-Za-z0-9_-]+$/);
	if (opts.pin) await expect(page.getByLabel(/^PIN/)).toHaveValue('');
	return link;
}

/** Opens a link in the viewer as a recipient would: the viewer URL with the link after '#'. */
async function openInViewer(viewer: Page, link: string, pin?: string) {
	// a new document each time: the viewer reads the link once, at load, and a hash change alone keeps it
	await viewer.goto('about:blank');
	await viewer.goto(`${VIEWER}/#${link}`);
	if (pin) {
		await expect(viewer.getByText('This SMART Health Link requires a passcode.')).toBeVisible();
		await viewer.locator('input[type=password]').fill(pin);
		await viewer.getByRole('button', { name: 'Submit' }).click();
	}
}

/** Decodes the QR code on the page with the viewer's own QR reader (qr-scanner, on its origin). */
async function readQr(page: Page, viewer: Page): Promise<string> {
	const svg = await page.locator('svg.sl-qr').evaluate((el) => new XMLSerializer().serializeToString(el));
	await viewer.goto(`${VIEWER}/captureQR.html`);
	return viewer.evaluate(async (markup) => {
		const img = new Image();
		img.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(markup);
		await img.decode();
		const canvas = document.createElement('canvas');
		canvas.width = canvas.height = 900;
		canvas.getContext('2d')?.drawImage(img, 0, 0, 900, 900);
		const scanner = (window as unknown as { QrScanner: { scanImage(c: HTMLCanvasElement, o: object): Promise<{ data: string }> } }).QrScanner;
		return (await scanner.scanImage(canvas, { returnDetailedScanResult: true })).data;
	}, svg);
}

function psql(sql: string) {
	execFileSync('docker', ['exec', 'ownhealth-testpg', 'psql', '-U', 'postgres', '-d', DB, '-qAtc', sql], { encoding: 'utf8' });
}

test.beforeEach(async ({ context, page }) => {
	seen = watch(context, page);
	await offline(context);
	// every test starts on Alex's own profile (the dev login is Alex: owner of himself, viewer of Sam)
	await page.goto('/?lang=en');
	await page.evaluate(() => localStorage.setItem('oh.profile', 'alex'));
});

test('a link with a PIN opens in the viewer, the owner sees who opened it, and after revoke it gives 404', async ({ context, page, request }) => {
	const pin = secret();
	const label = 'For Dr. Ortega (e2e PIN)';
	await page.goto('/links?lang=en');
	await expect(page.getByRole('heading', { name: 'Share links', level: 1 })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Share links' })).toBeVisible();
	// the live region is in the page before the link is; the summary in Spanish from an English page
	const live = page.locator('.sr-only[aria-live="polite"]');
	await expect(live).toHaveCount(1);
	await expect(live).toHaveText('');
	await page.getByLabel('Language of the summary').selectOption({ label: 'Español' });
	const link = await makeLink(page, { label, pin, make: 'Create link', ready: /The link “For Dr. Ortega \(e2e PIN\)” is ready/ });
	await expect(page.getByRole('heading', { name: /The link “For Dr\. Ortega/ })).toBeFocused();
	await expect(live).toHaveText('The link “For Dr. Ortega (e2e PIN)” is ready');
	const p = payload(link);
	expect(p.flag).toBe('P');
	expect(p.label).toBe(label);
	expect(p.url).toMatch(/^http:\/\/127\.0\.0\.1:\d+\/api\/shl\/[A-Za-z0-9_-]{43}$/);
	await expect(page.getByText('This link asks for its PIN.')).toBeVisible();

	// the QR code is the link itself
	const viewer = await context.newPage();
	expect(await readQr(page, viewer)).toBe(link);

	// a recipient opens it with the PIN; the viewer's manifest answer names the one-hour file location
	let location = '';
	viewer.on('response', async (r) => {
		if (r.request().method() === 'POST' && r.url() === p.url && r.status() === 200) location = (await r.json()).files[0].location;
	});
	await openInViewer(viewer, link, pin);
	await expect(viewer.getByText('Alex Rivera').first()).toBeVisible();
	await expect(viewer.getByText('Resumen internacional del paciente').first()).toBeVisible();
	expect(location).toMatch(/\/api\/shl\/[A-Za-z0-9_-]{43}\/file\/[A-Za-z0-9_-]{43}$/);

	// the owner sees the opening in the access log: the viewer's own name, the manifest and the file
	const row = page.getByTestId('shl-row').filter({ hasText: label });
	await page.reload();
	await expect(row.getByText('opened once')).toBeVisible();
	await row.getByRole('button', { name: `Who opened it · ${label}` }).click();
	await expect(row.getByRole('cell', { name: 'Opened the link' })).toBeVisible();
	await expect(row.getByRole('cell', { name: 'Downloaded the summary' })).toBeVisible();
	await expect(row.getByRole('cell', { name: RECIPIENT }).first()).toBeVisible();

	// revoke ends access: the manifest and the file location the viewer got both answer 404
	await row.getByRole('button', { name: `Revoke · ${label}` }).click();
	await row.getByRole('button', { name: 'Revoke', exact: true }).click();
	await expect(row.getByText('Revoked')).toBeVisible();
	await expect(row.getByRole('button', { name: `Revoke · ${label}` })).toHaveCount(0);
	// the keyboard stays on the row, not on the top of the page
	await expect(row.getByRole('button', { name: `Who opened it · ${label}` })).toBeFocused();
	expect((await request.post(p.url, { data: { recipient: 'e2e', passcode: pin } })).status()).toBe(404);
	expect((await request.get(location)).status()).toBe(404);
	await openInViewer(viewer, link, pin);
	await expect(viewer.getByText('The SHL is no longer active.')).toBeVisible();
	await expect(viewer.getByText('Alex Rivera')).toHaveCount(0);

	// the PIN, the link and its key never travelled in a URL, a console, the history or the storage
	expect(page.url()).toBe(`${new URL(page.url()).origin}/links?lang=en`);
	await assertNoLeak(page, link, pin);
});

test('past its expiry a link gives 404 and the viewer says it is no longer active (Spanish page)', async ({ context, page, request }) => {
	const label = 'Caduca (e2e)';
	await page.goto('/links?lang=es');
	await expect(page.getByRole('heading', { name: 'Enlaces para compartir', level: 1 })).toBeVisible();
	await page.getByLabel('Caduca en').selectOption({ label: '1 día' });
	await page.getByLabel('Idioma del resumen').selectOption({ label: 'English' });
	const link = await makeLink(page, { label, make: 'Crear enlace', ready: /El enlace «Caduca \(e2e\)» está listo/ });
	const p = payload(link);
	expect(p.flag).toBeUndefined();
	// one day from now, as the server writes it into the link
	expect(Math.abs(p.exp - (Date.now() / 1000 + 86_400))).toBeLessThan(120);

	const viewer = await context.newPage();
	await openInViewer(viewer, link);
	await expect(viewer.getByText('Alex Rivera').first()).toBeVisible();
	await expect(viewer.getByText('International Patient Summary').first()).toBeVisible();

	// the day passes: the link's expiry moves into the past on the server, the link it handed out does not
	const items = (await (await request.get('/api/profiles/alex/shl')).json()).items as { id: string; label: string }[];
	const id = items.find((x) => x.label === label)?.id ?? '';
	expect(id).toMatch(/^[0-9a-f-]{36}$/);
	psql(`update shl set expires_at = now() - interval '1 second' where id = '${id}'`);

	expect((await request.post(p.url, { data: { recipient: 'e2e' } })).status()).toBe(404);
	await openInViewer(viewer, link);
	await expect(viewer.getByText('The SHL is no longer active.')).toBeVisible();
	await page.reload();
	const row = page.getByTestId('shl-row').filter({ hasText: label });
	await expect(row.getByText('Caducado')).toBeVisible();
	await expect(row.getByText(/^creado el .* · caducó el /)).toBeVisible();
	await expect(row.getByRole('button', { name: `Revocar · ${label}` })).toHaveCount(0);
	await assertNoLeak(page, link);
});

test('control: the same link with a wrong key shows the viewer error, never the summary', async ({ context, page }) => {
	await page.goto('/links?lang=en');
	const link = await makeLink(page, { label: 'Wrong key (e2e)', make: 'Create link', ready: /is ready/ });
	const wrong = withKey(link, btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(32)))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, ''));
	expect(wrong).not.toBe(link);
	const viewer = await context.newPage();
	// the right key opens it, so the failure below is the key alone
	await openInViewer(viewer, link);
	await expect(viewer.getByText('Alex Rivera').first()).toBeVisible();
	await openInViewer(viewer, wrong);
	await expect(viewer.getByText(/decrypt|operation failed/i)).toBeVisible();
	await expect(viewer.getByText('Alex Rivera')).toHaveCount(0);
});

test('control: the leak check exempts only the viewer page and the exact SHL endpoint of the app', async ({ baseURL }) => {
	const id = 'a'.repeat(43);
	const manifest = new URL(`/api/shl/${id}`, baseURL).href;
	const file = `${manifest}/file/${'b'.repeat(43)}`;
	expect(exempt(manifest, manifest)).toBe(true);
	expect(exempt(file, manifest)).toBe(true);
	expect(exempt(`${VIEWER}/#shlink:/x`, manifest)).toBe(true);
	const other = new URL(manifest);
	other.port = String(Number(other.port) + 1);
	for (const u of [
		`https://collector.invalid/api/shl/${id}`,
		`https://collector.invalid/api/shl/${id}/file/${'b'.repeat(43)}`,
		other.href,
		`${manifest}?copy=1`,
		new URL(`/elsewhere/api/shl/${id}`, baseURL).href,
		`${manifest}/file/${'b'.repeat(43)}/more`,
		`${VIEWER}/collect?u=${id}`
	])
		expect(exempt(u, manifest), u).toBe(false);
});

test('a viewer of a profile gets no form, the server refuses a link anyway, and a made link leaves with the profile', async ({ page, request }) => {
	await page.goto('/links?lang=en');
	await makeLink(page, { label: 'Switch (e2e)', make: 'Create link', ready: /is ready/ });
	await page.getByRole('button', { name: 'Sam Rivera', exact: true }).click();
	await expect(page.getByText('Only the owner or a guardian of Sam Rivera can share this profile.')).toBeVisible();
	await expect(page.getByRole('button', { name: 'Create link' })).toHaveCount(0);
	const res = await request.post('/api/profiles/sam/shl', { data: {} });
	expect(res.status()).toBe(403);
	expect((await res.json()).detail.code).toBe('not_sharer');
	// back on Alex, the link made a moment ago is never shown again: only the list row remains
	await page.getByRole('button', { name: 'Alex Rivera', exact: true }).click();
	await expect(page.getByTestId('shl-row').filter({ hasText: 'Switch (e2e)' })).toBeVisible();
	await expect(page.getByTestId('shl-link')).toHaveCount(0);
	await expect(page.locator('svg.sl-qr')).toHaveCount(0);
});

test('a link made just before a profile switch never shows as the next profile\'s link', async ({ page }) => {
	await page.goto('/links?lang=en');
	await expect(page.getByRole('heading', { name: 'Links of Alex Rivera' })).toBeVisible();
	// hold the create answer until the person is on Lucía's page
	let release = () => {};
	const held = new Promise<void>((r) => (release = r));
	await page.route('**/api/profiles/alex/shl', async (route) => {
		if (route.request().method() === 'POST') await held;
		await route.fallback();
	});
	const label = 'Made for Alex, switched (e2e)';
	await page.getByLabel(/^Label/).fill(label);
	await page.getByLabel(/^PIN/).fill(secret());
	await page.getByRole('button', { name: 'Create link', exact: true }).click();
	await expect(page.getByRole('button', { name: 'Creating…' })).toBeVisible();
	await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Links of Lucía Rivera' })).toBeVisible();
	// nothing typed for Alex stays in Lucía's form
	await expect(page.getByLabel(/^Label/)).toHaveValue('');
	await expect(page.getByLabel(/^PIN/)).toHaveValue('');
	const answered = page.waitForResponse((r) => r.url().endsWith('/api/profiles/alex/shl') && r.request().method() === 'POST');
	release();
	expect((await answered).status()).toBe(201);
	// the page has handled the answer once the button is back
	await expect(page.getByRole('button', { name: 'Create link', exact: true })).toBeEnabled();
	await expect(page.getByTestId('shl-link')).toHaveCount(0);
	await expect(page.locator('svg.sl-qr')).toHaveCount(0);
	await expect(page.getByTestId('shl-row').filter({ hasText: label })).toHaveCount(0);
	await expect(page.locator('.sr-only[aria-live="polite"]')).toHaveText('');
	// the link is Alex's, in Alex's list
	await page.getByRole('button', { name: 'Alex Rivera', exact: true }).click();
	await expect(page.getByTestId('shl-row').filter({ hasText: label })).toBeVisible();
	await expect(page.getByTestId('shl-link')).toHaveCount(0);
});

test('the form offers only what the server accepts: a lock of the owner, a closed summary, a break-glass grant', async ({ page }) => {
	const lock = (locks: string[]) => psql(`update profile set locks = array[${locks.map((l) => `'${l}'`).join(',')}]::text[] where slug = 'alex'`);
	const glass = (on: boolean) =>
		psql(
			`update profile_grant set break_glass = ${on}, expires_at = ${on ? "now() + interval '14 days'" : 'null'} ` +
				`where profile_id = (select id from profile where slug = 'lucia') and user_id = (select user_id from profile where slug = 'alex')`
		);
	try {
		// the owner's own lock on lab results: not offered, never ticked, and the link made is accepted
		lock(['read:labs']);
		await page.goto('/links?lang=en');
		const labs = page.getByRole('checkbox', { name: /^Lab results/ });
		await expect(labs).toBeDisabled();
		await expect(labs).not.toBeChecked();
		await expect(page.getByText('· closed for you on this profile')).toHaveCount(1);
		await expect(page.getByRole('checkbox', { name: /^Records/ })).toBeEnabled();
		const label = 'Labs locked (e2e)';
		await makeLink(page, { label, make: 'Create link', ready: /is ready/ });
		await expect(page.getByTestId('shl-row').filter({ hasText: label }).getByText('Summary · Records')).toBeVisible();
		// the summary locked too: one sentence and no form
		lock(['read:labs', 'read:summary']);
		await page.reload();
		await expect(page.getByText('Every link carries the summary, and the summary of Alex Rivera is locked or outside your access, so you cannot share this profile.')).toBeVisible();
		await expect(page.getByRole('button', { name: 'Create link' })).toHaveCount(0);
		await expect(page.getByRole('heading', { name: 'Links of Alex Rivera' })).toBeVisible();
		lock([]);
		// a break-glass grant on Lucía: one sentence, no form, no refusal shown as an outage
		glass(true);
		await page.reload();
		await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
		await expect(page.getByText('Emergency access to Lucía Rivera does not include sharing this profile.')).toBeVisible();
		await expect(page.getByRole('button', { name: 'Create link' })).toHaveCount(0);
		await expect(page.getByText(/not_sharer|Retry/)).toHaveCount(0);
	} finally {
		lock([]);
		glass(false);
	}
});

test('someone who may share no profile has no Share links in the menu, and no profile at all reads one sentence', async ({ page }) => {
	// one route for the whole test; each step says how /me changes
	let change: (m: { profiles: { role: string; break_glass: boolean }[] }) => void = () => {};
	await page.route('**/api/me', async (route) => {
		const res = await route.fetch();
		const body = await res.json();
		change(body);
		await route.fulfill({ response: res, json: body });
	});
	const menu = page.locator('nav.menu a');
	// the menu draws before /me answers, so an absent item proves nothing until the household (from
	// /me) is on screen
	const loaded = () => expect(page.getByRole('button', { name: 'Alex Rivera', exact: true })).toBeVisible();
	// the control: Alex owns his own profile and sees the item
	await page.goto('/?lang=en');
	await loaded();
	await expect(menu.filter({ hasText: 'Share links' })).toHaveCount(1);
	// every profile only viewed
	change = (m) => m.profiles.forEach((p) => (p.role = 'viewer'));
	await page.reload();
	await loaded();
	await expect(menu.filter({ hasText: 'Share links' })).toHaveCount(0);
	// every grant a break-glass one
	change = (m) => m.profiles.forEach((p) => (p.break_glass = true));
	await page.reload();
	await loaded();
	await expect(menu.filter({ hasText: 'Share links' })).toHaveCount(0);
	// no profile: the page says so instead of showing a bare heading
	change = (m) => (m.profiles = []);
	await page.goto('/links?lang=en');
	await expect(page.getByText('You have no profiles to share.')).toBeVisible();
});
