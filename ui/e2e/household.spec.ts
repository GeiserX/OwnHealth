// The household overview on the home page: one row per member the signed-in person can see, and
// each row's counts are that member's own: flags from the status, overdue items from the Due list,
// stale values as the member's own dashboard marks them, as the API answers them for the same
// person. A person with no grant on a member gets no row for them; a grant without the summary
// shows the row as not shared, never as zeros; a person who sees one profile gets no overview.
import { expect, test, type APIRequestContext, type Browser, type Locator, type Page } from '@playwright/test';

const LANGS = [
	{ lang: 'es', title: 'El hogar de un vistazo', locked: 'No compartido' },
	{ lang: 'en', title: 'Household at a glance', locked: 'Not shared' }
] as const;

type Expected = { flags: number; overdue: number; stale: number };
type Card = { withheld?: boolean; markers: { stale: boolean }[] };

/** What the API says about one member, for whoever `request` signs in as. */
async function expected(request: APIRequestContext, slug: string, lang: string): Promise<Expected> {
	const status = await (await request.get(`/api/profiles/${slug}/status?lang=${lang}`)).json();
	const due = await (await request.get(`/api/profiles/${slug}/due?lang=${lang}`)).json();
	const dash = await (await request.get(`/api/profiles/${slug}/dashboard?lang=${lang}`)).json();
	return {
		flags: status.data.flags.length,
		overdue: due.items.filter((i: { status: string }) => i.status === 'overdue').length,
		stale: (dash.systems as Card[]).filter((s) => !s.withheld).flatMap((s) => s.markers).filter((m) => m.stale).length
	};
}

const section = (page: Page) => page.getByRole('region', { name: /El hogar de un vistazo|Household at a glance/ });
const rowOf = (page: Page, name: string): Locator => section(page).getByRole('row').filter({ hasText: name });

async function expectCounts(row: Locator, want: Expected) {
	const cells = row.getByRole('cell');
	await expect(cells.nth(1)).toHaveText(String(want.flags));
	await expect(cells.nth(2)).toHaveText(String(want.overdue));
	await expect(cells.nth(3)).toHaveText(String(want.stale));
}

/** As the demo owner (dev sign-in): invite a new person, grant them `grants`, and sign them in. */
async function invited(browser: Browser, baseURL: string, grants: { profile: string; role: string; categories?: string[] }[]) {
	const owner = await browser.newContext({ baseURL });
	const headers = { Origin: baseURL };
	const invite = await owner.request.post('/api/household/invites', { headers, data: { display_name: `Pat Rivera ${Date.now()}`, locale: 'en' } });
	expect(invite.status()).toBe(201);
	const { user_id, code } = await invite.json();
	for (const grant of grants) {
		const res = await owner.request.put('/api/grants', { headers, data: { user_id, ...grant } });
		expect(res.status(), await res.text()).toBe(200);
	}
	await owner.close();
	// enrol with a password (this test server has no passkeys)
	const ctx = await browser.newContext({ baseURL, colorScheme: 'dark' });
	const enrol = await ctx.request.post('/api/auth/enroll/password', { headers, data: { code, password: 'synthetic-pass-phrase-54' } });
	expect(enrol.status(), await enrol.text()).toBe(200);
	return ctx;
}

for (const { lang, title } of LANGS) {
	test(`one row per member, with each member's own counts, in ${lang}`, async ({ page, baseURL }) => {
		const errors: string[] = [];
		page.on('pageerror', (e) => errors.push(e.message));
		const headers = { Origin: baseURL! };
		// Snooze one of Alex's overdue items first, so his Overdue and Stale counts differ and a row
		// that shows one column's number in the other cannot pass.
		const due = await (await page.request.get('/api/profiles/alex/due?lang=en')).json();
		const key: string = due.items.find((i: { status: string }) => i.status === 'overdue').key;
		const until = new Date(Date.now() + 30 * 86_400_000).toISOString().slice(0, 10);
		const snooze = await page.request.post(`/api/profiles/alex/due/${encodeURIComponent(key)}/decision`, { headers, data: { kind: 'snooze', until } });
		expect(snooze.ok(), await snooze.text()).toBe(true);
		try {
			const me = await (await page.request.get('/api/me')).json();
			const names: string[] = me.profiles.map((p: { display_name: string }) => p.display_name);
			expect(names.length).toBeGreaterThan(1);

			await page.goto(`/?lang=${lang}`);
			await expect(section(page).getByRole('heading', { name: title })).toBeVisible();
			// the region and its table are named by the title alone, not the subtitle beside it
			await expect(page.getByRole('region', { name: title, exact: true })).toBeVisible();
			await expect(section(page).getByRole('table', { name: title, exact: true })).toBeVisible();
			// header row + one row per member, no more
			await expect(section(page).getByRole('row')).toHaveCount(names.length + 1);
			let total = 0;
			let apart = 0;
			for (const p of me.profiles) {
				const want = await expected(page.request, p.slug, lang);
				total += want.flags + want.overdue + want.stale;
				if (want.overdue !== want.stale) apart++;
				await expectCounts(rowOf(page, p.display_name), want);
			}
			// the demo has flags, overdue items and stale values, so a row stuck at zero cannot pass,
			// and some member's Overdue differs from their Stale, so swapped columns cannot pass
			expect(total).toBeGreaterThan(0);
			expect(apart).toBeGreaterThan(0);
			expect(errors).toEqual([]);
		} finally {
			await page.request.delete(`/api/profiles/alex/due/${encodeURIComponent(key)}/decision`, { headers });
		}
	});
}

test("a member's Stale count is the stale values on their own dashboard, wearables included", async ({ page }) => {
	await page.goto('/?lang=en');
	const row = rowOf(page, 'Lucía Rivera');
	await expect(row.getByRole('cell').nth(3)).toHaveText(/^\d+$/);
	const shown = Number(await row.getByRole('cell').nth(3).textContent());
	// Lucía's labs are recent; her wearables are not. The status lists lab recency only.
	const status = await (await page.request.get('/api/profiles/lucia/status?lang=en')).json();
	expect(shown).toBeGreaterThan(status.data.stale.length);
	// her own dashboard marks the same number of values stale
	await row.getByRole('button', { name: 'Open Lucía Rivera', exact: true }).click();
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(/Lucía Rivera/);
	await expect(page.locator('.dash .stale-b')).toHaveCount(shown);
});

test('a row opens that member on the home page', async ({ page }) => {
	await page.goto('/?lang=en');
	const open = rowOf(page, 'Carmen Ortega').getByRole('button', { name: 'Open Carmen Ortega', exact: true });
	await open.click();
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(/Carmen Ortega/);
	await expect(open).toHaveAttribute('aria-current', 'true');
});

test('a person sees rows only for the members their grants cover', async ({ browser, baseURL }) => {
	// two of the four members, one of them without the summary
	const pat = await invited(browser, baseURL!, [
		{ profile: 'alex', role: 'viewer' },
		{ profile: 'lucia', role: 'viewer', categories: ['read:labs'] }
	]);
	const page = await pat.newPage();
	for (const { lang, locked } of LANGS) {
		await page.goto(`/?lang=${lang}`);
		await expect(section(page).getByRole('row')).toHaveCount(3);
		await expect(rowOf(page, 'Sam Rivera')).toHaveCount(0);
		await expect(rowOf(page, 'Carmen Ortega')).toHaveCount(0);
		await expectCounts(rowOf(page, 'Alex Rivera'), await expected(page.request, 'alex', lang));
		// a viewer holds the summary only: the row says what it leaves out
		await expect(rowOf(page, 'Alex Rivera')).toContainText('read:labs');
		// no summary at all: the row is not shared, and shows no counts
		const lucia = rowOf(page, 'Lucía Rivera');
		await expect(lucia).toContainText(locked);
		await expect(lucia.getByRole('cell').nth(1)).toHaveText('—');
	}
	await pat.close();
});

test('a person who sees one profile gets its dashboard alone, with no overview', async ({ browser, baseURL }) => {
	const solo = await invited(browser, baseURL!, [{ profile: 'alex', role: 'viewer' }]);
	const page = await solo.newPage();
	const calls: string[] = [];
	page.on('request', (r) => /\/api\/profiles\/[^/]+\/status/.test(r.url()) && calls.push(r.url()));
	await page.goto('/?lang=en');
	// the page has loaded the one profile, and the overview is not there
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(/Alex Rivera/);
	await expect(page.locator('.dash')).toBeVisible();
	await expect(section(page)).toHaveCount(0);
	expect(calls).toEqual([]);
	await solo.close();
});

test('a member whose status fails says so and retries that row alone', async ({ page }) => {
	let fail = true;
	await page.route('**/api/profiles/sam/status*', (route) => (fail ? route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":{"code":"internal","message":"boom"}}' }) : route.continue()));
	await page.goto('/?lang=en');
	const sam = rowOf(page, 'Sam Rivera');
	await expect(sam).toContainText('Could not load');
	await expect(sam).toContainText('The server did not answer (HTTP 500');
	await expect(sam.getByRole('cell').nth(1)).toHaveText('—');
	// the other members still show their counts
	for (const name of ['Alex Rivera', 'Lucía Rivera', 'Carmen Ortega']) await expect(rowOf(page, name).getByRole('cell').nth(1)).toHaveText(/^\d+$/);
	fail = false;
	const calls: string[] = [];
	page.on('request', (r) => /\/api\/profiles\/[^/]+\/(status|dashboard)/.test(r.url()) && calls.push(new URL(r.url()).pathname));
	await sam.getByRole('button', { name: 'Retry' }).click();
	await expectCounts(sam, await expected(page.request, 'sam', 'en'));
	await expect(sam).not.toContainText('Could not load');
	expect(calls.sort()).toEqual(['/api/profiles/sam/dashboard', '/api/profiles/sam/status']);
});

test('on a phone the note on a partial row wraps inside the card, in es', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto('/?lang=es');
	const note = rowOf(page, 'Sam Rivera').getByText(/Parcial: necesita/);
	await expect(note).toBeVisible();
	const card = await section(page).boundingBox();
	const box = await note.boundingBox();
	expect(card && box).toBeTruthy();
	expect(box!.x).toBeGreaterThanOrEqual(card!.x);
	expect(box!.x + box!.width).toBeLessThanOrEqual(card!.x + card!.width);
	// and nothing in the card is clipped or scrolls sideways
	const [scroll, client] = await section(page).evaluate((el) => [el.scrollWidth, el.clientWidth]);
	expect(scroll).toBeLessThanOrEqual(client);
});
