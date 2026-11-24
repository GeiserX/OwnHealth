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
	const probe = await browser.newContext({ baseURL });
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
	// leave no contact, open request or unread notice behind for the specs that run after this one
	await owner.request.delete(`/api/profiles/alex/break-glass/contacts/${patId}`, { headers: headers(baseURL!) });
	await owner.request.post('/api/notifications/read', { headers: headers(baseURL!), data: {} });
	await owner.close();
	await pat.close();
});

test('a contact asks, the owner cancels from the calendar link, and nothing opens', async ({ baseURL }) => {
	const page = await owner.newPage();
	const errors = pageErrors(page);
	await page.goto('/family?lang=en');
	const card = page.getByTestId('bg-owner');
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

	// the contact asks, with a reason
	const pp = await pat.newPage();
	const patErrors = pageErrors(pp);
	await pp.goto('/family?lang=en');
	const mine = pp.getByTestId('bg-contact-card');
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

	// every read through the request is in the owner's notices, as one line with its count
	const notices = await (await owner.request.get('/api/notifications')).json();
	const uses = notices.items.filter((n: { kind: string; request_id: string }) => n.kind === 'break_glass.use' && n.request_id === second).length;
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
