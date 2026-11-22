// The language picker reads the registry (src/lib/i18n/langs.ts): one button per language, the page
// switches as a whole, and the choice survives a reload. A new language adds one row here.
import { expect, test } from '@playwright/test';

const LANGS = [
	{ code: 'es', label: 'ES', name: 'Español', group: 'Idioma', household: 'Hogar' },
	{ code: 'en', label: 'EN', name: 'English', group: 'Language', household: 'Household' }
];

test('the picker lists every registry language and switches the page to each', async ({ page }) => {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	await page.goto(`/?lang=${LANGS[0].code}`);
	for (const l of [...LANGS.slice(1), LANGS[0]]) {
		const picker = page.locator('header .seg[role=group]');
		await expect(picker.getByRole('button')).toHaveText(LANGS.map((x) => x.label));
		await picker.getByRole('button', { name: l.label, exact: true }).click();
		await expect(page.locator('html')).toHaveAttribute('lang', l.code);
		await expect(page.getByRole('group', { name: l.group })).toBeVisible();
		await expect(page.locator('.side .plabel').first()).toHaveText(l.household);
		for (const x of LANGS) {
			const b = picker.getByRole('button', { name: x.label, exact: true });
			await expect(b).toHaveAttribute('aria-pressed', String(x.code === l.code));
			await expect(b).toHaveAttribute('title', x.name);
		}
		// The choice is saved: a reload without ?lang keeps it.
		await page.goto('/');
		await expect(page.locator('html')).toHaveAttribute('lang', l.code);
	}
	expect(errors).toEqual([]);
});

// The two language choices that reach the server send registry codes: the family invite (the new
// login's locale) and the summary sheet. Each offers every language once, by code, named in itself.
test('the family invite offers every registry language by its code', async ({ page }) => {
	await page.goto('/family?lang=en');
	await page.locator('.ph .right .btn.pri').click();
	const invite = page.locator('form[aria-labelledby=fm-inv-h] select').first();
	await expect(invite).toBeVisible();
	const options = await invite.evaluate((s: HTMLSelectElement) => [...s.options].map((o) => [o.value, o.text.trim()]));
	expect(options).toEqual(LANGS.map((l) => [l.code, l.name]));
});

test('the summary sheet offers every registry language by its code', async ({ page }) => {
	const want = LANGS.map((l) => [l.code, l.name]);
	await page.goto('/summary?lang=en');
	const radios = page.locator('input[type=radio][name=lang]');
	await expect(radios).toHaveCount(LANGS.length);
	const values = await radios.evaluateAll((rs) => rs.map((r) => [(r as HTMLInputElement).value, r.parentElement?.textContent?.trim() ?? '']));
	expect(values).toEqual(want);
});
