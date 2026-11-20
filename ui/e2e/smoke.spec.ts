// Browser smoke test: the built UI, served by the real server on a fresh demo database, opens the
// synthetic Rivera household in both languages on the dark theme, without a page error.
import { expect, test, type Page } from '@playwright/test';

const PROFILES = ['Alex Rivera', 'Sam Rivera', 'Lucía Rivera', 'Carmen Ortega'];

const LANGS = [
	{ lang: 'es', household: 'Hogar', synthetic: 'Solo datos sintéticos.' },
	{ lang: 'en', household: 'Household', synthetic: 'Synthetic data only.' }
];

function collectPageErrors(page: Page): string[] {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	return errors;
}

for (const { lang, household, synthetic } of LANGS) {
	test(`the demo household opens in ${lang} on the dark theme`, async ({ page }) => {
		const errors = collectPageErrors(page);
		await page.goto(`/?lang=${lang}`);
		await expect(page.locator('html')).toHaveAttribute('lang', lang);
		await expect(page.locator('.side .plabel').first()).toHaveText(household);
		for (const name of PROFILES) {
			await expect(page.getByRole('button', { name, exact: true })).toBeVisible();
		}
		await expect(page.getByText(synthetic)).toBeVisible();
		// Dark theme: the body paints an opaque dark background (alpha 1, every channel under 64 of
		// 255). A transparent body would show whatever is under it, so it fails here.
		const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
		const [r, g, b, a = 1] = (bg.match(/[\d.]+/g) ?? []).map(Number);
		expect(a, bg).toBe(1);
		for (const c of [r, g, b]) expect(c, bg).toBeLessThan(64);
		expect(errors).toEqual([]);
	});
}

// The control: once the page has loaded, a selector that matches nothing must fail. If it ever
// passes, the run turns red, so a harness that cannot fail is caught on every run. Only the last
// step is expected to fail: a page that does not load turns the run red too.
test('control: a selector that matches nothing fails', async ({ page }) => {
	await page.goto('/?lang=en');
	await expect(page.getByRole('button', { name: 'Alex Rivera', exact: true })).toBeVisible();
	test.fail();
	await expect(page.locator('#no-such-element')).toBeVisible({ timeout: 2_000 });
});
