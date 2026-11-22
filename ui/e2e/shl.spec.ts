// Share links (slice 52): a link made on /links opens in a reference SMART Health Links viewer
// (shc-web-reader, pinned in shl-viewer.sh, served on its own origin), with its PIN when it has one.
// After revoke, and after its expiry, the same link gives 404 and the viewer says so. The red control:
// the same link with a wrong key shows the viewer's error, never the summary. The PIN and the link's
// key never reach a URL or a console of either page, and the QR code decodes to the link. Requests
// to anything but this machine are refused, so no step depends on the internet.
import { execFileSync } from 'node:child_process';
import { expect, test, type BrowserContext, type Page } from '@playwright/test';

const VIEWER = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_VIEWER_PORT}`;
const DB = process.env.OWNHEALTH_E2E_DB ?? '';
// the name the viewer gives itself in the manifest request (src/lib/SHX.js SHL_RECIPIENT)
const RECIPIENT = 'SMART Health Card Web Reader';

/** A fresh secret for this run: no PIN is written in the repository. */
const secret = () => `pin-${Math.random().toString(36).slice(2, 10)}`;

/** Every request URL and console line of the context, to prove no secret reaches them. */
function watch(context: BrowserContext): { urls: string[]; logs: string[] } {
	const seen = { urls: [] as string[], logs: [] as string[] };
	context.on('request', (r) => seen.urls.push(r.url()));
	context.on('page', (p) => p.on('console', (m) => seen.logs.push(m.text())));
	return seen;
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
	await offline(context);
	// every test starts on Alex's own profile (the dev login is Alex: owner of himself, viewer of Sam)
	await page.goto('/?lang=en');
	await page.evaluate(() => localStorage.setItem('oh.profile', 'alex'));
});

test('a link with a PIN opens in the viewer, the owner sees who opened it, and after revoke it gives 404', async ({ context, page, request }) => {
	const seen = watch(context);
	const pin = secret();
	const label = 'For Dr. Ortega (e2e PIN)';
	await page.goto('/links?lang=en');
	await expect(page.getByRole('heading', { name: 'Share links', level: 1 })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Share links' })).toBeVisible();
	const link = await makeLink(page, { label, pin, make: 'Create link', ready: /The link “For Dr. Ortega \(e2e PIN\)” is ready/ });
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
	expect((await request.post(p.url, { data: { recipient: 'e2e', passcode: pin } })).status()).toBe(404);
	expect((await request.get(location)).status()).toBe(404);
	await openInViewer(viewer, link, pin);
	await expect(viewer.getByText('The SHL is no longer active.')).toBeVisible();
	await expect(viewer.getByText('Alex Rivera')).toHaveCount(0);

	// the PIN and the link's key never travelled in a URL or reached a console
	const leaks = (s: string) => s.includes(pin) || s.includes(p.key);
	expect(seen.urls.filter(leaks)).toEqual([]);
	expect(seen.logs.filter(leaks)).toEqual([]);
	expect(page.url()).toBe(`${new URL(page.url()).origin}/links?lang=en`);
});

test('past its expiry a link gives 404 and the viewer says it is no longer active (Spanish page)', async ({ context, page, request }) => {
	const label = 'Caduca (e2e)';
	await page.goto('/links?lang=es');
	await expect(page.getByRole('heading', { name: 'Enlaces para compartir', level: 1 })).toBeVisible();
	await page.getByLabel('Caduca en').selectOption({ label: '1 día' });
	const link = await makeLink(page, { label, make: 'Crear enlace', ready: /El enlace «Caduca \(e2e\)» está listo/ });
	const p = payload(link);
	expect(p.flag).toBeUndefined();
	// one day from now, as the server writes it into the link
	expect(Math.abs(p.exp - (Date.now() / 1000 + 86_400))).toBeLessThan(120);

	const viewer = await context.newPage();
	await openInViewer(viewer, link);
	await expect(viewer.getByText('Alex Rivera').first()).toBeVisible();

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
	await expect(row.getByRole('button', { name: `Revocar · ${label}` })).toHaveCount(0);
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
