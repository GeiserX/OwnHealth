// LibreView and Dexcom Clarity CSV exports through the upload panel (slice 28): what an export brought
// shows under the file in each registry language, the same file again adds nothing, and the sensor
// appears on the wearables screen. Synthetic exports for Lucía Rivera (fixtures/cgm).
import { expect, test, type Page } from '@playwright/test';
import { fileURLToPath } from 'node:url';

// the browser's zone reads the export's wall-clock times: the e2e server has no OWNHEALTH_TIMEZONE
test.use({ timezoneId: 'Europe/Madrid' });

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
			'Times read in Europe/Madrid'
		],
		{ timeout: 60_000 }
	);
	lines = await upload(page, 'en', 'clarity-en-mgdl.csv');
	await expect(lines.getByRole('listitem').first()).toHaveText('0 glucose readings imported', { timeout: 60_000 });
	await expect(lines.getByRole('listitem').nth(1)).toHaveText('8 already stored: not added again');
	// the document screen says what the export is and what it holds, after a reload too
	await page.locator('section.up').getByRole('link', { name: 'clarity-en-mgdl.csv' }).first().click();
	await page.reload();
	await expect(page.getByText('A CGM export from Dexcom Clarity. Its glucose readings are on Wearables: it has no pages and no rows to review.')).toBeVisible();
	await expect(page.getByRole('list', { name: 'CGM export' }).getByRole('listitem')).toHaveText([
		'8 glucose readings from this file',
		'Sensor: Dexcom G7',
		'Times read in Europe/Madrid'
	]);
	await expect(page.getByText('Glucose readings need no review: they are on Wearables.')).toBeVisible();
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
			'Horas leídas en Europe/Madrid'
		],
		{ timeout: 60_000 }
	);
});

test('the picker takes the same file again, and another profile gets an empty upload panel', async ({ page }) => {
	const lines = await upload(page, 'en', 'libreview-en-mgdl.csv');
	await expect(lines.getByRole('listitem').first()).toBeVisible({ timeout: 60_000 });
	// the picker is empty again once it handed its files over, so the same file can be picked again
	await expect(page.locator('section.up input[type=file]').first()).toHaveValue('');
	// another profile with the panel open: Lucía's rows and links stay with Lucía
	await page.getByRole('button', { name: 'Alex Rivera', exact: true }).click();
	await expect(page.locator('section.up h3')).toContainText('Alex Rivera');
	await expect(page.locator('section.up table')).toHaveCount(0);
	await expect(page.locator('section.up').getByRole('link', { name: 'libreview-en-mgdl.csv' })).toHaveCount(0);
});

test.describe('a browser that reports an old zone name', () => {
	// Chromium names India Asia/Calcutta, which the Debian Postgres image does not ship
	test.use({ timezoneId: 'Asia/Kolkata' });

	test('uploads, and the export is read in the current name of its zone', async ({ page }) => {
		const lines = await upload(page, 'en', 'clarity-de-mmol.csv');
		const zone = await page.evaluate(() => Intl.DateTimeFormat().resolvedOptions().timeZone);
		expect(zone, 'this Chromium reports the current name: the old-name case is no longer covered').toBe('Asia/Calcutta');
		await expect(lines.getByText('Times read in Asia/Kolkata', { exact: true })).toBeVisible({ timeout: 60_000 });
	});
});
