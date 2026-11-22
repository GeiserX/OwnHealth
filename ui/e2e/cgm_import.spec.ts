// LibreView and Dexcom Clarity CSV exports through the upload panel (slice 28): what an export brought
// shows under the file in each registry language, the same file again adds nothing, and the sensor
// appears on the wearables screen. Synthetic exports for Lucía Rivera (fixtures/cgm).
import { expect, test, type Page } from '@playwright/test';
import { fileURLToPath } from 'node:url';

const fixture = (name: string) => fileURLToPath(new URL(`../../fixtures/cgm/${name}`, import.meta.url));

/** Upload a fixture to Lucía from the import screen; returns the list of result lines of that file. */
async function upload(page: Page, lang: 'en' | 'es', name: string) {
	await page.goto(`/import-review?lang=${lang}`);
	await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
	const open = page.getByRole('button', { name: lang === 'en' ? 'Upload' : 'Subir', exact: true });
	if ((await open.getAttribute('aria-expanded')) !== 'true') await open.click();
	await page.locator('section.up input[type=file]').first().setInputFiles(fixture(name));
	return page.getByRole('list', { name });
}

test('a Dexcom Clarity export shows what it brought, and the same file again adds nothing', async ({ page }) => {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	let lines = await upload(page, 'en', 'clarity-en-mgdl.csv');
	await expect(page.locator('section.up').getByText('Dexcom Clarity CSV', { exact: true })).toBeVisible();
	await expect(lines.getByRole('listitem')).toHaveText(
		[
			'8 glucose readings imported',
			"1 marked Low: below the sensor's range, so there is no number to store",
			"1 marked High: above the sensor's range, so there is no number to store",
			'2 rows that are not glucose readings (insulin, food, notes, calibrations) were left out',
			'Times read as UTC: no time zone is set for this server (OWNHEALTH_TIMEZONE)'
		],
		{ timeout: 60_000 }
	);
	lines = await upload(page, 'en', 'clarity-en-mgdl.csv');
	await expect(lines.getByRole('listitem').first()).toHaveText('0 glucose readings imported', { timeout: 60_000 });
	await expect(lines.getByRole('listitem').nth(1)).toHaveText('8 already stored: not added again');
	// the sensor is a device of Lucía's on the wearables screen, named by the export
	await page.goto('/wearables?lang=en');
	await expect(page.getByText('Dexcom G7', { exact: true })).toBeVisible();
	expect(errors).toEqual([]);
});

test('a LibreView export in Spanish names the column it did not read', async ({ page }) => {
	const lines = await upload(page, 'es', 'libreview-es-mgdl.csv');
	await expect(page.locator('section.up').getByText('CSV de LibreView', { exact: true })).toBeVisible();
	await expect(lines.getByRole('listitem')).toHaveText(
		[
			'6 lecturas de glucosa importadas',
			'1 columna sin leer: Glucosa del sensor adicional mg/dL',
			'Horas leídas como UTC: este servidor no tiene zona horaria (OWNHEALTH_TIMEZONE)'
		],
		{ timeout: 60_000 }
	);
});
