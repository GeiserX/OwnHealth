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
		// Dark theme: the page background stays dark (every channel under 64 of 255).
		const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
		const channels = (bg.match(/\d+/g) ?? []).slice(0, 3).map(Number);
		expect(channels, bg).toHaveLength(3);
		for (const c of channels) expect(c, bg).toBeLessThan(64);
		expect(errors).toEqual([]);
	});
}

// The control: a selector that matches nothing must fail. If it ever passes, the run turns red,
// so a harness that cannot see the page (or cannot fail) is caught on every run.
test('control: a selector that matches nothing fails', async ({ page }) => {
	test.fail();
	await page.goto('/?lang=en');
	await expect(page.getByRole('button', { name: 'Alex Rivera', exact: true })).toBeVisible();
	await expect(page.locator('#no-such-element')).toBeVisible({ timeout: 2_000 });
});
