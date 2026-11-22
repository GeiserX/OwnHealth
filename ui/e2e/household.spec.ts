// The household overview on the home page: one row per member the signed-in person can see, and
// each row's counts are that member's own status (flags, stale values) and Due list (overdue
// items), as the API answers them for the same person. A person with no grant on a member gets no
// row for them; a grant without the summary shows the row as not shared, never as zeros.
import { expect, test, type APIRequestContext, type Locator, type Page } from '@playwright/test';

const LANGS = [
	{ lang: 'es', title: 'El hogar de un vistazo', locked: 'No compartido' },
	{ lang: 'en', title: 'Household at a glance', locked: 'Not shared' }
] as const;

type Expected = { flags: number; overdue: number; stale: number };

/** What the API says about one member, for whoever `request` signs in as. */
async function expected(request: APIRequestContext, slug: string, lang: string): Promise<Expected> {
	const status = await (await request.get(`/api/profiles/${slug}/status?lang=${lang}`)).json();
	const due = await (await request.get(`/api/profiles/${slug}/due?lang=${lang}`)).json();
	return {
		flags: status.data.flags.length,
		overdue: due.items.filter((i: { status: string }) => i.status === 'overdue').length,
		stale: status.data.stale.length
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

for (const { lang, title } of LANGS) {
	test(`one row per member, with each member's own counts, in ${lang}`, async ({ page }) => {
		const errors: string[] = [];
		page.on('pageerror', (e) => errors.push(e.message));
		const me = await (await page.request.get('/api/me')).json();
		const names: string[] = me.profiles.map((p: { display_name: string }) => p.display_name);
		expect(names.length).toBeGreaterThan(1);

		await page.goto(`/?lang=${lang}`);
		await expect(section(page).getByRole('heading', { name: title })).toBeVisible();
		// header row + one row per member, no more
		await expect(section(page).getByRole('row')).toHaveCount(names.length + 1);
		let total = 0;
		for (const p of me.profiles) {
			const want = await expected(page.request, p.slug, lang);
			total += want.flags + want.overdue + want.stale;
			await expectCounts(rowOf(page, p.display_name), want);
		}
		// the demo has flags, overdue items and stale values, so a row stuck at zero cannot pass
		expect(total).toBeGreaterThan(0);
		expect(errors).toEqual([]);
	});
}

test('a row opens that member on the home page', async ({ page }) => {
	await page.goto('/?lang=en');
	const open = rowOf(page, 'Carmen Ortega').getByRole('button', { name: 'Open Carmen Ortega', exact: true });
	await open.click();
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(/Carmen Ortega/);
	await expect(open).toHaveAttribute('aria-current', 'true');
});

test('a person sees rows only for the members their grants cover', async ({ browser, baseURL }) => {
	// As the demo owner (dev sign-in): invite a new person and grant them two of the four members,
	// one of them without the summary.
	const owner = await browser.newContext({ baseURL });
	const headers = { Origin: baseURL! };
	const invite = await owner.request.post('/api/household/invites', { headers, data: { display_name: `Pat Rivera ${Date.now()}`, locale: 'en' } });
	expect(invite.status()).toBe(201);
	const { user_id, code } = await invite.json();
	for (const grant of [
		{ profile: 'alex', role: 'viewer' },
		{ profile: 'lucia', role: 'viewer', categories: ['read:labs'] }
	]) {
		const res = await owner.request.put('/api/grants', { headers, data: { user_id, ...grant } });
		expect(res.status(), await res.text()).toBe(200);
	}
	await owner.close();

	// As that person: enrol with a password (this test server has no passkeys), then open home.
	const pat = await browser.newContext({ baseURL, colorScheme: 'dark' });
	const enrol = await pat.request.post('/api/auth/enroll/password', { headers, data: { code, password: 'synthetic-pass-phrase-54' } });
	expect(enrol.status(), await enrol.text()).toBe(200);
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
