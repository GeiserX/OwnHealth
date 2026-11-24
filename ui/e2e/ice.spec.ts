import { expect, test } from '@playwright/test';

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
	await row.getByRole('button', { name: 'Revoke · Emergency card' }).click();
	await row.getByRole('button', { name: 'Revoke', exact: true }).click();
	await expect(row).toContainText('Revoked');
});

test('the emergency card form and downloads fit a Spanish phone screen', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto('/summary?lang=es');
	const card = page.getByRole('region', { name: 'Tarjeta de emergencia' });
	await expect(card).toBeVisible();
	await card.getByRole('button', { name: 'Crear tarjeta de emergencia' }).click();
	await expect(card.getByRole('link', { name: 'PDF para la cartera' })).toBeVisible();
	await expect(card.getByRole('link', { name: 'PNG para la pantalla de bloqueo' })).toBeVisible();
	expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(390);
});
