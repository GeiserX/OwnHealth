import { expect, test } from '@playwright/test';
// The registry languages (src/lib/i18n/langs.ts), as langs.spec.ts lists them.
const LANGS = [
	{ code: 'es', name: 'Español' },
	{ code: 'en', name: 'English' }
];

test('the owner can download an emergency card and end its online access', async ({ page }) => {
	await page.goto('/summary?lang=en');
	const card = page.getByRole('region', { name: 'Emergency card' });
	await expect(card).toBeVisible();
	await card.getByLabel('Contact name').fill('Carmen Ortega');
	await card.getByLabel('Phone number').fill('+34 555 010 020');
	await card.getByLabel('Add a QR for the live summary').check();
	await card.getByRole('button', { name: 'Create emergency card' }).click();
	const pdf = page.waitForEvent('download');
	await card.getByRole('link', { name: 'Wallet PDF' }).click();
	expect((await pdf).suggestedFilename()).toBe('ownhealth-ice.pdf');
	const png = page.waitForEvent('download');
	await card.getByRole('link', { name: 'Lock-screen PNG' }).click();
	expect((await png).suggestedFilename()).toBe('ownhealth-ice.png');
	await card.getByRole('link', { name: 'Manage cards and links' }).click();
	const row = page.getByTestId('shl-row').filter({ hasText: 'Emergency card' }).first();
	await expect(row).toContainText('Online summary');
	await expect(row.getByRole('button', { name: 'Who opened it · Emergency card' })).toBeVisible();
	await row.getByRole('button', { name: 'Revoke · Emergency card' }).click();
	await row.getByRole('button', { name: 'Revoke', exact: true }).click();
	await expect(row).toContainText('Revoked');
});

test('the card has its own language and the owner can leave any record off it', async ({ page }) => {
	await page.goto('/summary?lang=en');
	const card = page.getByRole('region', { name: 'Emergency card' });
	await expect(card).toBeVisible();
	// the doctor sheet's language does not choose the card's
	await page.locator('input[type=radio][name=lang][value=es]').check();
	const select = card.locator('select[name=ice-lang]');
	await expect(select).toHaveValue('en');
	const options = await select.evaluate((s: HTMLSelectElement) => [...s.options].map((o) => [o.value, o.text.trim()]));
	expect(options).toEqual(LANGS.map((l) => [l.code, l.name]));
	await expect(card.locator('#ice-until')).toHaveText(/^Expires on \d{4}-\d{2}-\d{2} at 00:00 UTC$/);
	await card.getByText('On the card').click();
	const first = card.locator('details').first().getByRole('checkbox').first();
	await expect(first).toBeChecked();
	await first.uncheck();
	const sent = page.waitForRequest((r) => r.method() === 'POST' && r.url().endsWith('/ice'));
	await card.getByRole('button', { name: 'Create emergency card' }).click();
	const body = (await sent).postDataJSON();
	expect(body.lang).toBe('en');
	expect(body.left_out).toHaveLength(1);
	await expect(card.getByRole('link', { name: 'Wallet PDF' })).toBeVisible();
});

test('the emergency card form and downloads fit a Spanish phone screen', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto('/summary?lang=es');
	const card = page.getByRole('region', { name: 'Tarjeta de emergencia' });
	await expect(card).toBeVisible();
	await card.getByRole('button', { name: 'Crear tarjeta de emergencia' }).click();
	await expect(card.getByRole('link', { name: 'PDF para la cartera' })).toBeVisible();
	await expect(card.getByRole('link', { name: 'PNG para la pantalla de bloqueo' })).toBeVisible();
	// focus lands on the result, which gives the expiry with its year and says the files are not kept
	const ready = card.locator('.result [role=status]');
	await expect(ready).toBeFocused();
	await expect(ready).toHaveText(/^Tarjeta de emergencia lista · Caduca el \d{4}-\d{2}-\d{2} a las 00:00 UTC$/);
	await expect(card).toContainText('Descarga los dos archivos ahora.');
	expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(390);
	// a second card replaces the first; an offline card has no access log to open
	await card.getByRole('button', { name: 'Crear tarjeta de emergencia' }).click();
	await expect(ready).toBeFocused();
	await card.getByRole('link', { name: 'Gestionar tarjetas y enlaces' }).click();
	const rows = page.getByTestId('shl-row').filter({ hasText: 'Tarjeta de emergencia' });
	await expect(rows.first()).toContainText('Tarjeta sin conexión');
	await expect(rows.first().getByRole('button', { name: /^Quién lo abrió/ })).toHaveCount(0);
	await expect(rows.nth(1)).toContainText('sustituida por una tarjeta nueva');
});
