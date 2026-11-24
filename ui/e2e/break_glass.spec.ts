// Break-glass in the browser (routes/break_glass.rs, slice 49): the owner names a contact, the
// contact asks, the owner follows the cancel link of the calendar event to the cancel screen and
// cancels, and nothing opens. A second request waits for the owner's calendar past its delay, opens
// once the feed served it, and each read through it reaches the owner's notices, which open by
// themselves at the next login, grouped per request, until they are read. A cancel on a page left
// open after the request moved on says so in the reader's language and shows the step that fits now.
// A request is moved 49 hours back with psql in the database serve.sh made, so its 48-hour delay has
// passed, as the minute sweep would find it.
import { expect, test, type Browser, type BrowserContext, type Page } from '@playwright/test';
import { execFileSync } from 'node:child_process';

const db = process.env.OWNHEALTH_E2E_DB ?? '';
const SHOWN = 'oh.notices.shown';

function sql(q: string): string {
	return execFileSync('docker', ['exec', 'ownhealth-testpg', 'psql', '-U', 'postgres', '-d', db, '-qAtc', q], { encoding: 'utf8' }).trim();
}

function pageErrors(page: Page): string[] {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	return errors;
}

/** The calendar text with its folded lines joined (RFC 5545 3.1). */
const unfold = (ics: string) => ics.replace(/\r?\n[ \t]/g, '');

test.describe.configure({ mode: 'serial' });

let owner: BrowserContext; // the demo owner of Alex's profile (dev sign-in)
let ownerId = '';
let pat: BrowserContext; // a login of the household with no grant on Alex's profile
let patId = '';
let patName = '';
let feed = '';
let first = '';
let second = '';
const headers = (baseURL: string) => ({ Origin: baseURL });

/** A context whose notices count as shown already, so the panel does not open over the page. */
async function quiet(browser: Browser, baseURL: string, user: string): Promise<BrowserContext> {
	const ctx = await browser.newContext({ baseURL, colorScheme: 'dark' });
	await ctx.addInitScript(([k, v]) => localStorage.setItem(k, v), [SHOWN, user]);
	return ctx;
}

test.beforeAll(async ({ browser, baseURL }) => {
	expect(db, 'the config names the database').not.toBe('');
	const probe = await browser.newContext({ baseURL, colorScheme: 'dark' });
	ownerId = (await (await probe.request.get('/api/me')).json()).user.id;
	patName = `Pat Rivera ${Date.now()}`;
	const invite = await probe.request.post('/api/household/invites', { headers: headers(baseURL!), data: { display_name: patName, locale: 'en' } });
	expect(invite.status(), await invite.text()).toBe(201);
	const made = await invite.json();
	patId = made.user_id;
	await probe.close();
	owner = await quiet(browser, baseURL!, ownerId);
	pat = await quiet(browser, baseURL!, patId);
	// a password login counts as a fresh confirmation for five minutes: Pat's request needs one
	const enrol = await pat.request.post('/api/auth/enroll/password', { headers: headers(baseURL!), data: { code: made.code, password: 'synthetic-pass-phrase-49' } });
	expect(enrol.status(), await enrol.text()).toBe(200);
});

test.afterAll(async ({ baseURL }) => {
	// leave no contact, open request or unread notice behind for the specs that run after this one: try
	// both, close both browsers whatever happens, and fail when either did not work
	const failed: string[] = [];
	const attempt = async (what: string, call: () => Promise<{ ok(): boolean; status(): number; text(): Promise<string> }>, gone = false) => {
		try {
			const res = await call();
			// a contact never named (a test stopped before it) leaves nothing behind either
			if (!res.ok() && !(gone && res.status() === 404)) failed.push(`${what}: ${res.status()} ${await res.text()}`);
		} catch (e) {
			failed.push(`${what}: ${e}`);
		}
	};
	try {
		await attempt('remove the contact', () => owner.request.delete(`/api/profiles/alex/break-glass/contacts/${patId}`, { headers: headers(baseURL!) }), true);
		await attempt('mark the notices read', () => owner.request.post('/api/notifications/read', { headers: headers(baseURL!), data: {} }));
	} finally {
		await owner?.close();
		await pat?.close();
	}
	expect(failed).toEqual([]);
});

test('a contact asks, the owner cancels from the calendar link, and nothing opens', async ({ baseURL }) => {
	const page = await owner.newPage();
	const errors = pageErrors(page);
	// only an owner reads the notices: this page does (the control of Pat's check below)
	let ownerReads = 0;
	page.on('request', (r) => {
		if (new URL(r.url()).pathname === '/api/notifications') ownerReads++;
	});
	await page.goto('/family?lang=en');
	const card = page.getByTestId('bg-owner');
	// access opens once the calendar app fetched the request; the app cannot know whether anyone saw it
	await expect(card).toContainText('Access opens only after the waiting time and once your calendar app has fetched the request.');
	// no calendar feed of the owner yet: naming a contact waits for one
	await expect(card.getByTestId('bg-need-feed')).toBeVisible();
	await expect(card.getByRole('button', { name: 'Name a contact' })).toHaveCount(0);
	const made = await owner.request.post('/api/profiles/alex/calendar-feeds', { headers: headers(baseURL!), data: { detail: 'generic' } });
	expect(made.status(), await made.text()).toBe(201);
	feed = (await made.json()).path;
	await page.reload();
	await expect(card.getByTestId('bg-need-feed')).toHaveCount(0);
	await card.getByRole('button', { name: 'Name a contact' }).click();
	// the form takes the keyboard; a member who cannot log in yet is listed, but cannot be picked
	await expect(card.getByLabel('Person')).toBeFocused();
	const household = await (await owner.request.get('/api/household')).json();
	const noLogin = household.members.find(
		(m: { user_id: string; own_profile?: string | null; logins?: { passkeys: number; oidc: boolean; password: boolean } }) =>
			m.user_id !== ownerId && m.own_profile !== 'alex' && m.logins && !m.logins.passkeys && !m.logins.oidc && !m.logins.password
	);
	expect(noLogin, 'the demo household has a member who cannot log in').toBeTruthy();
	await expect(card.getByRole('option', { name: `${noLogin.display_name} (cannot log in yet)` })).toBeDisabled();
	await card.getByLabel('Person').selectOption({ label: patName });
	await card.getByRole('button', { name: 'Save' }).click();
	const contact = card.getByTestId('bg-contact').filter({ hasText: patName });
	await expect(contact).toContainText('after 48 hours');
	await expect(contact).toContainText('Read lab results');

	// the contact asks, with a reason. Pat owns no profile, so Pat's pages never read notices.
	const pp = await pat.newPage();
	const patErrors = pageErrors(pp);
	let patReads = 0;
	pp.on('request', (r) => {
		if (new URL(r.url()).pathname === '/api/notifications') patReads++;
	});
	// the first read of the card fails: it says so and offers a retry, where a list nobody named would
	// show nothing
	let failOnce = true;
	await pp.route('**/api/break-glass', (route) => {
		if (!failOnce || route.request().method() !== 'GET') return route.continue();
		failOnce = false;
		return route.fulfill({ status: 500, json: { detail: { code: 'internal', message: 'Internal error' } } });
	});
	await pp.goto('/family?lang=en');
	const mine = pp.getByTestId('bg-contact-card');
	await expect(mine).toContainText('Emergency access you can ask for');
	await expect(mine).not.toContainText('Alex Rivera');
	await mine.getByRole('button', { name: 'Retry' }).click();
	await pp.unroute('**/api/break-glass');
	await expect(mine).toContainText('Alex Rivera');
	await mine.getByRole('button', { name: 'Ask for access' }).click();
	await mine.getByLabel('Why you need access').fill('Alex is in hospital after a fall');
	await mine.getByRole('button', { name: 'Send the request' }).click();
	await expect(mine.getByRole('status')).toContainText('Waiting. Access opens on');
	// one request at a time: no second Ask while it waits; the keyboard is on where it stands
	await expect(mine.getByRole('button', { name: 'Ask for access' })).toHaveCount(0);
	await expect(mine.getByRole('status')).toBeFocused();
	first = sql(`select id from break_glass_request where contact_user_id = '${patId}' and status = 'requested'`);
	expect(first).toMatch(/^[0-9a-f-]{36}$/);

	// the owner sees it on the family page, and the badge counts it
	await page.reload();
	const req = card.getByTestId('bg-request');
	await expect(req).toContainText(patName);
	await expect(req).toContainText('Alex is in hospital after a fall');
	await expect(req.getByRole('status')).toContainText('Waiting. Access opens on');
	await expect(page.getByTestId('nt-badge')).toHaveText('1');

	// Details opens the cancel screen of that request
	await req.getByRole('link', { name: 'Details' }).click();
	await expect(page).toHaveURL(new RegExp(`/family/break-glass/${first}$`));
	await expect(page.getByTestId('bgc-status')).toContainText('Waiting.');

	// the calendar event carries the cancel link, which lands on the cancel screen; it takes no
	// second confirmation, since cancelling only narrows access
	const ics = unfold(await (await owner.request.get(feed)).text());
	const link = `${baseURL}/family/break-glass/${first}`;
	expect(ics).toContain(link);
	const stepUps: string[] = [];
	page.on('request', (r) => {
		if (r.url().includes('/api/auth/step-up')) stepUps.push(r.url());
	});
	// a login confirmed long ago: a step-up gate would ask now, so the empty list below can fail
	await page.route('**/api/auth/state', async (route) => {
		const res = await route.fetch();
		await route.fulfill({ response: res, json: { ...(await res.json()), step_up_fresh: false } });
	});
	await page.goto(`${link}?lang=en`);
	await expect(page.getByRole('heading', { level: 1 })).toHaveText('Emergency access request');
	await expect(page.getByText(`${patName} asked for access to Alex Rivera`)).toBeVisible();
	await expect(page.getByText('Alex is in hospital after a fall')).toBeVisible();
	await page.getByRole('button', { name: 'Cancel the request' }).click();
	// the second step says what happens; Go back leaves it waiting
	await expect(page.getByRole('group', { name: `Cancel ${patName}'s request? Access will not open.` })).toBeVisible();
	await page.getByRole('button', { name: 'Cancel the request' }).click();
	await expect(page.getByText('The request is cancelled. Access will not open.')).toBeVisible();
	await expect(page.getByTestId('bgc-status')).toContainText('Cancelled on');
	expect(stepUps).toEqual([]);

	// its delay passes: a cancelled request never opens
	sql(`update break_glass_request set requested_at = requested_at - interval '49 hours', activates_at = activates_at - interval '49 hours' where id = '${first}'`);
	await owner.request.get(feed);
	await pp.reload();
	await expect(mine.getByRole('status')).toContainText('Cancelled on');
	expect(sql(`select status from break_glass_request where id = '${first}'`)).toBe('cancelled');
	expect((await pat.request.get('/api/profiles/alex/status')).status()).not.toBe(200);
	const me = await (await pat.request.get('/api/me')).json();
	expect(me.profiles.map((p: { slug: string }) => p.slug)).not.toContain('alex');
	expect(errors).toEqual([]);
	expect(patErrors).toEqual([]);
	expect(ownerReads).toBeGreaterThan(0);
	expect(patReads).toBe(0);
	await pp.close();
	await page.close();
});

test('a second request waits for the owner calendar past its delay, opens once it was fetched, and each read reaches the owner', async () => {
	const pp = await pat.newPage();
	const patErrors = pageErrors(pp);
	await pp.goto('/family?lang=en');
	const mine = pp.getByTestId('bg-contact-card');
	await mine.getByRole('button', { name: 'Ask for access' }).click();
	await mine.getByLabel('Why you need access').fill('Second try: Alex is still in hospital');
	await mine.getByRole('button', { name: 'Send the request' }).click();
	await expect(mine.getByRole('status')).toContainText('Waiting.');
	second = sql(`select id from break_glass_request where contact_user_id = '${patId}' and status = 'requested'`);
	expect(second).toMatch(/^[0-9a-f-]{36}$/);

	// the delay passes before any calendar app fetched the feed: it waits for the owner calendar
	sql(`update break_glass_request set requested_at = requested_at - interval '49 hours', activates_at = activates_at - interval '49 hours' where id = '${second}'`);
	for (const [lang, contactLine] of [
		['en', "the owner's calendar app has not fetched the request yet"],
		['es', 'la app de calendario de la persona titular aún no ha recogido la solicitud']
	]) {
		await pp.goto(`/family?lang=${lang}`);
		await expect(mine.getByRole('status')).toContainText(contactLine);
	}
	const page = await owner.newPage();
	const errors = pageErrors(page);
	const ownerLines = ['your calendar app has not fetched the request yet', 'tu app de calendario aún no ha recogido la solicitud'];
	for (const [lang, ownerLine] of [
		['en', ownerLines[0]],
		['es', ownerLines[1]]
	]) {
		await page.goto(`/family?lang=${lang}`);
		await expect(page.getByTestId('bg-request').getByRole('status')).toContainText(ownerLine);
	}
	await expect(page.getByTestId('bg-owner')).toContainText('cuando tu app de calendario ha recogido la solicitud.');
	expect(sql(`select status from break_glass_request where id = '${second}'`)).toBe('requested');

	// the owner's calendar app fetches the feed: the next request of a person opens it
	await owner.request.get(feed);
	await pp.goto('/family?lang=en');
	await expect(mine.getByRole('status')).toContainText('Open until');

	// the owner's page still shows it waiting: a cancel now says, in Spanish, that it changed, and the
	// row shows it open with the step that fits it
	const row = page.getByTestId('bg-request');
	await expect(row.getByRole('status')).toContainText(ownerLines[1]);
	await row.getByRole('button', { name: 'Cancelar la solicitud' }).click();
	await row.getByRole('group').getByRole('button', { name: 'Cancelar la solicitud' }).click();
	await expect(row.getByRole('alert')).toHaveText('Esta solicitud cambió mientras la página estaba abierta. El estado que se ve es el actual.');
	await expect(row.getByRole('status')).toContainText('Abierto hasta el');
	await expect(row.getByRole('button', { name: 'Terminar el acceso' })).toBeVisible();
	await expect(row.getByRole('button', { name: 'Cancelar la solicitud' })).toHaveCount(0);
	expect(sql(`select status from break_glass_request where id = '${second}'`)).toBe('active');

	await mine.getByRole('button', { name: "Open Alex Rivera's records" }).click();
	await expect(pp.getByRole('heading', { level: 1 })).toContainText('Alex Rivera');
	await pp.goto('/biomarkers?lang=en');
	await expect(pp.getByRole('heading', { level: 1 })).toBeVisible();
	expect(patErrors).toEqual([]);
	await pp.close();

	// every read through the request is in the owner's notices, as one line with the server's count
	const notices = await (await owner.request.get('/api/notifications')).json();
	const uses = notices.uses.find((u: { request_id: string }) => u.request_id === second).count;
	expect(uses).toBeGreaterThan(1);
	await page.goto('/family?lang=en');
	await page.getByRole('button', { name: /^Notices/ }).click();
	const panel = page.getByRole('region', { name: /^Notices/ });
	const entry = panel.getByRole('listitem').filter({ hasText: 'access opened' });
	await expect(entry).toContainText(`${patName} · Alex Rivera`);
	await expect(entry).toContainText(`read through it ${uses} times`);
	// the first request is its own entry: asked, then cancelled by the owner
	await expect(panel.getByRole('listitem').filter({ hasText: 'cancelled by' })).toContainText('asked for access');
	await expect(panel.getByRole('listitem')).toHaveCount(2);
	await expect(page.getByTestId('nt-badge')).toHaveText('2');
	// See the request opens its cancel screen
	await entry.getByRole('link', { name: 'See the request' }).click();
	await expect(page).toHaveURL(new RegExp(`/family/break-glass/${second}$`));
	await expect(page.getByTestId('bgc-status')).toContainText('Open until');
	expect(errors).toEqual([]);
	await page.close();
});

test('an end on a cancel screen left open after another owner ended it says so and offers nothing more', async ({ baseURL }) => {
	const page = await owner.newPage();
	const errors = pageErrors(page);
	await page.goto(`/family/break-glass/${second}?lang=es`);
	await expect(page.getByTestId('bgc-status')).toContainText('Abierto hasta el');
	// ended elsewhere while this page stays open
	const ended = await owner.request.post(`/api/profiles/alex/break-glass/requests/${second}/revoke`, { headers: headers(baseURL!) });
	expect(ended.status(), await ended.text()).toBe(200);
	await page.getByRole('button', { name: 'Terminar el acceso' }).click();
	await page.getByRole('group').getByRole('button', { name: 'Terminar el acceso' }).click();
	await expect(page.getByRole('alert')).toHaveText('Esta solicitud cambió mientras la página estaba abierta. El estado que se ve es el actual.');
	await expect(page.getByTestId('bgc-status')).toContainText('Terminada por una persona titular el');
	await expect(page.getByTestId('bgc-step')).toHaveCount(0);
	expect(errors).toEqual([]);
	await page.close();
});

test('a request whose delay runs out while the pages are open shows open at the next tick, for the contact and the owner', async ({ baseURL }) => {
	// Pat asks a third time. Leave enough waiting time for all three pages to load.
	const asked = await pat.request.post('/api/break-glass/requests', { headers: headers(baseURL!), data: { profile: 'alex', reason: 'Third try: Alex is back in hospital' } });
	expect(asked.status(), await asked.text()).toBe(201);
	const third = sql(`select id from break_glass_request where contact_user_id = '${patId}' and status = 'requested'`);
	expect(third).toMatch(/^[0-9a-f-]{36}$/);
	const setupMinutes = 10;
	sql(`update break_glass_request set activates_at = now() + make_interval(mins => ${setupMinutes}) where id = '${third}'`);
	expect((await owner.request.get(feed)).status()).toBe(200);
	// three pages left open on it, each with a clock the test moves on: the contact's card, the owner's
	// card and the cancel screen
	const pp = await pat.newPage();
	const page = await owner.newPage();
	const cs = await owner.newPage();
	const errors = [pageErrors(pp), pageErrors(page), pageErrors(cs)];
	for (const p of [pp, page, cs]) await p.clock.install();
	await pp.goto('/family?lang=en');
	const mine = pp.getByTestId('bg-contact-card');
	await expect(mine.getByRole('status')).toContainText('Waiting. Access opens on');
	await page.goto('/family?lang=en');
	const row = page.getByTestId('bg-request');
	await expect(row.getByRole('status')).toContainText('Waiting. Access opens on');
	await cs.goto(`/family/break-glass/${third}?lang=en`);
	await expect(cs.getByTestId('bgc-status')).toContainText('Waiting.');
	// Lose the connection for one focus refresh. The next scheduled read should still work.
	await pp.route('**/api/break-glass', (route) => route.abort('internetdisconnected'), { times: 1 });
	const failedRefresh = pp.waitForEvent('requestfailed', (r) => new URL(r.url()).pathname === '/api/break-glass');
	await pp.evaluate(() => window.dispatchEvent(new Event('focus')));
	await failedRefresh;
	await expect(mine.getByRole('status')).toContainText('Waiting. Access opens on');
	// Use the earlier calendar fetch as the deadline, then advance the owner pages past the deadline they read.
	sql(`update break_glass_request set activates_at = owner_feed_at where id = '${third}'`);
	const pastSetupDeadline = (setupMinutes + 1) * 60_000;
	await page.clock.fastForward(pastSetupDeadline);
	await expect(row.getByRole('status')).toContainText('Open until');
	await expect(row.getByRole('button', { name: 'End access' })).toBeVisible();
	await cs.clock.fastForward(pastSetupDeadline);
	await expect(cs.getByTestId('bgc-status')).toContainText('Open until');
	await pp.clock.fastForward('01:01');
	await expect(mine.getByRole('status')).toContainText('Open until');
	await expect(mine.getByRole('button', { name: "Open Alex Rivera's records" })).toBeVisible();
	expect(sql(`select status from break_glass_request where id = '${third}'`)).toBe('active');
	for (const e of errors) expect(e).toEqual([]);
	// the owner ends it; the contact's card follows at its next refresh, and then stops reading
	const ended = await owner.request.post(`/api/profiles/alex/break-glass/requests/${third}/revoke`, { headers: headers(baseURL!) });
	expect(ended.status(), await ended.text()).toBe(200);
	let reads = 0;
	pp.on('request', (r) => {
		if (new URL(r.url()).pathname === '/api/break-glass') reads++;
	});
	await pp.clock.fastForward('01:01');
	await expect(mine.getByRole('status')).toContainText('Ended by an owner on');
	await expect(mine.getByRole('button', { name: "Open Alex Rivera's records" })).toHaveCount(0);
	await pp.clock.fastForward('05:00');
	await pp.clock.fastForward('05:00');
	expect(reads).toBe(1);
	for (const p of [pp, page, cs]) await p.close();
});

test('a first notices load that fails opens them at the next refresh, with the read count of the server', async ({ browser, baseURL }) => {
	const ctx = await browser.newContext({ baseURL, colorScheme: 'dark' });
	const page = await ctx.newPage();
	const errors = pageErrors(page);
	await page.clock.install();
	const me = await (await ctx.request.get('/api/me')).json();
	const alex = me.profiles.find((p: { slug: string }) => p.slug === 'alex');
	const profile = { id: alex.id, slug: 'alex', display_name: alex.display_name };
	const contact = { user_id: '00000000-0000-4000-8000-000000000049', display_name: 'Sam Fixture' };
	const at = (s: number) => new Date(Date.now() - s * 1000).toISOString();
	const notice = (id: string, kind: string, s: number) => ({ id, kind, profile, request_id: 'r-fixture', actor: null, contact, created_at: at(s), read_at: null });
	// the list holds two of the request's reads; the server counted three
	const fixture = {
		items: [notice('n-open', 'break_glass.activate', 60), notice('n-use-1', 'break_glass.use', 30), notice('n-use-2', 'break_glass.use', 20)],
		uses: [{ request_id: 'r-fixture', profile, contact, count: 3, unread: 3, last_at: at(20) }],
		unread: 4
	};
	let reads = 0;
	await page.route('**/api/notifications', (route) => {
		reads++;
		if (reads === 1) return route.fulfill({ status: 500, json: { detail: { code: 'internal', message: 'Internal error' } } });
		return route.fulfill({ json: fixture });
	});
	await page.goto('/?lang=en');
	await expect.poll(() => reads).toBe(1);
	await expect(page.getByRole('button', { name: /^Notices/ })).toHaveCount(0);
	// the tab comes back a minute later: the read works now, and the unread notices open by themselves
	await page.clock.fastForward('01:01');
	await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
	const panel = page.getByRole('region', { name: /^Notices/ });
	await expect(panel).toBeVisible();
	await expect(panel.getByRole('listitem')).toContainText('Sam Fixture · Alex Rivera');
	await expect(panel.getByRole('listitem')).toContainText('read through it 3 times');
	// Mark all as read, then the keyboard moves on before the answer: the answer leaves it where it went
	let release = () => {};
	const gate = new Promise<void>((r) => (release = r));
	await page.route('**/api/notifications/read', async (route) => {
		await gate;
		await route.fulfill({ json: { read: 0 } });
	});
	const readAll = panel.getByRole('button', { name: 'Mark all as read' });
	await readAll.click();
	const es = page.locator('button[lang="es"]').first();
	await es.focus();
	release();
	await expect.poll(() => reads).toBe(3);
	await expect(readAll).toBeEnabled();
	await page.waitForTimeout(300);
	await expect(es).toBeFocused();
	await expect(panel).toBeVisible();
	expect(errors).toEqual([]);
	await ctx.close();
});

test('unread notices open at the next login, in es and en, until they are read', async ({ browser, baseURL }) => {
	for (const [lang, title, uses] of [
		['en', 'Notices', 'read through it'],
		['es', 'Avisos', 'se leyó a través de ella']
	]) {
		// a new browser session: the notices open by themselves
		const ctx = await browser.newContext({ baseURL, colorScheme: 'dark' });
		const page = await ctx.newPage();
		const errors = pageErrors(page);
		await page.goto(`/?lang=${lang}`);
		const panel = page.getByRole('region', { name: new RegExp(`^${title}`) });
		await expect(panel).toBeVisible();
		await expect(panel).toContainText(uses);
		// once per login: the next page, and another tab, of the same login leave it closed
		await page.goto(`/family?lang=${lang}`);
		await expect(page.getByTestId('bg-owner')).toBeVisible();
		await expect(panel).toHaveCount(0);
		await expect(page.getByTestId('nt-badge')).toBeVisible();
		const tab = await ctx.newPage();
		await tab.goto(`/family?lang=${lang}`);
		await expect(tab.getByTestId('bg-owner')).toBeVisible();
		await expect(tab.getByRole('region', { name: new RegExp(`^${title}`) })).toHaveCount(0);
		await tab.close();
		// the session ends (a logout, an expired login) and the next login in this same tab opens them again
		await page.route('**/api/me', (route) => route.fulfill({ status: 401, json: { detail: { code: 'unauthenticated', message: 'Not logged in.' } } }));
		await page.goto(`/?lang=${lang}`);
		await expect.poll(() => page.evaluate((k) => localStorage.getItem(k), SHOWN)).toBeNull();
		await page.unroute('**/api/me');
		await page.goto(`/?lang=${lang}`);
		await expect(panel).toBeVisible();
		expect(errors).toEqual([]);
		await ctx.close();
	}
	// the session ends mid-use, on any call of a page: the next login in this tab opens them again
	const mid = await browser.newContext({ baseURL, colorScheme: 'dark' });
	const mp = await mid.newPage();
	await mp.goto('/?lang=en');
	await expect(mp.getByRole('region', { name: /^Notices/ })).toBeVisible();
	await mp.route('**/api/household', (route) => route.fulfill({ status: 401, json: { detail: { code: 'unauthenticated', message: 'Not logged in.' } } }));
	await mp.goto('/family?lang=en');
	await expect.poll(() => mp.evaluate((k) => localStorage.getItem(k), SHOWN)).toBeNull();
	await mp.unroute('**/api/household');
	await mp.goto('/?lang=en');
	await expect(mp.getByRole('region', { name: /^Notices/ })).toBeVisible();
	await mid.close();
	const ctx = await browser.newContext({ baseURL, colorScheme: 'dark' });
	const page = await ctx.newPage();
	await page.goto('/?lang=en');
	const panel = page.getByRole('region', { name: /^Notices/ });
	await panel.getByRole('button', { name: 'Mark all as read' }).click();
	await expect(page.getByTestId('nt-badge')).toHaveCount(0);
	// the button went with the news; the keyboard stays in the panel
	await expect(panel.getByRole('button', { name: 'Close' })).toBeFocused();
	await ctx.close();
	// read: the next login opens nothing, and the notices stay one click away
	const next = await browser.newContext({ baseURL, colorScheme: 'dark' });
	const again = await next.newPage();
	await again.goto('/?lang=en');
	await expect(again.getByRole('button', { name: 'Notices', exact: true })).toBeVisible();
	await expect(again.getByRole('region', { name: /^Notices/ })).toHaveCount(0);
	await next.close();
});
