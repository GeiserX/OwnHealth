// A vaccine entered by hand with its dose number (Cronología > Registros > Vacuna > Añadir a mano).
// The dose is a number input, and Svelte binds a number to it, not text: the form must read it as a
// number when it saves, and again when a correction changes it.
import { expect, test } from '@playwright/test';

test('a vaccine with a dose number saves by hand, and a correction changes the dose', async ({ page }) => {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	const vaccine = `Tétanos e2e ${Date.now()}`;
	await page.goto('/timeline?lang=es');
	await expect(page.locator('html')).toHaveAttribute('lang', 'es');
	const card = page.locator('.card').filter({ has: page.getByRole('heading', { name: /^Registros/ }) });
	await card.getByRole('group', { name: 'Qué añadir' }).getByRole('button', { name: 'Vacuna', exact: true }).click();
	await card.getByRole('button', { name: 'Añadir a mano' }).click();

	const add = card.getByRole('form', { name: 'Añadir a mano' });
	await add.getByLabel('Vacuna', { exact: true }).fill(vaccine);
	await add.getByLabel('Fecha', { exact: true }).fill('2024-05-02');
	await add.getByLabel('N.º de dosis').fill('1');
	await add.getByRole('button', { name: 'Guardar', exact: true }).click();
	// saved: the form closes and the record is in the list, with no error shown
	await expect(add).toHaveCount(0);
	const row = card.locator('div', { has: page.getByText(vaccine, { exact: true }) }).filter({ has: page.getByRole('button', { name: 'Corregir' }) }).last();
	await expect(row).toContainText('2024-05-02');

	// The correction form pre-fills the dose as text; changing it binds a number again.
	await row.getByRole('button', { name: 'Corregir' }).click();
	const fix = card.getByRole('form', { name: 'Corregir' });
	await expect(fix.getByLabel('N.º de dosis')).toHaveValue('1');
	await fix.getByLabel('N.º de dosis').fill('2');
	await fix.getByLabel('Motivo del cambio').fill('Segunda dosis');
	await fix.getByRole('button', { name: 'Guardar como versión nueva' }).click();
	await expect(fix).toHaveCount(0);
	await row.getByRole('button', { name: 'Corregir' }).click();
	await expect(card.getByRole('form', { name: 'Corregir' }).getByLabel('N.º de dosis')).toHaveValue('2');
	await expect(card.getByRole('alert')).toHaveCount(0);
	expect(errors).toEqual([]);
});
