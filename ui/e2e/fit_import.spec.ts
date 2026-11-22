// FIT activity files through the upload panel (slice 29): what a run brought shows under the file in each
// registry language, with the note that its route was not stored; the same file again adds nothing; a
// ride's distance is left out; a cut file and a course are refused with our sentence; and the watch is a
// device on the wearables screen. The files are synthetic, written by the server tests' FIT writer
// (fixtures/fit, checked against it by ownhealth-devices tests/fit.rs).
import { expect, test, type Page } from '@playwright/test';

const UPLOAD = { en: 'Upload', es: 'Subir' } as const;

/** Upload a fixture to Lucía from the import screen; returns the row of that file. */
async function upload(page: Page, lang: keyof typeof UPLOAD, name: string) {
	await page.goto(`/import-review?lang=${lang}`);
	await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
	const open = page.getByRole('button', { name: UPLOAD[lang], exact: true });
	if ((await open.getAttribute('aria-expanded')) !== 'true') await open.click();
	// Playwright reads a relative path from its working folder, ui/
	await page.locator('section.up input[type=file]').first().setInputFiles(`../fixtures/fit/${name}`);
	return page.locator('section.up tr', { hasText: name });
}

test('a run shows what it brought and that its route was left out, and the same file again adds nothing', async ({ page }) => {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	await upload(page, 'en', 'lucia-run-gps.fit');
	await expect(page.locator('section.up').getByText('FIT activity', { exact: true })).toBeVisible();
	const lines = page.getByRole('list', { name: 'lucia-run-gps.fit' }).getByRole('listitem');
	await expect(lines).toHaveText(
		[
			'1 activity read',
			'30 minutes of heart rate imported',
			'1 walking or running distance imported',
			'1 activity energy total imported',
			'Device: OwnHealth Test Watch',
			'The route (GPS positions) in the file was not stored'
		],
		{ timeout: 60_000 }
	);
	await page.screenshot({ path: 'test-results/fit-run-en.png', fullPage: true });
	await upload(page, 'en', 'lucia-run-gps.fit');
	const again = page.getByRole('list', { name: 'lucia-run-gps.fit' }).getByRole('listitem');
	await expect(again.nth(1)).toHaveText('0 minutes of heart rate imported', { timeout: 60_000 });
	await expect(again.nth(4)).toHaveText('32 already stored: not added again');
	// the watch is one of Lucía's devices, named by the file
	await page.goto('/wearables?lang=en');
	await expect(page.getByText('OwnHealth Test Watch').first()).toBeVisible();
	expect(errors).toEqual([]);
});

test('a gzipped ride in Spanish brings heart rate and energy, and says why its distance is left out', async ({ page }) => {
	await upload(page, 'es', 'lucia-ride.fit.gz');
	await expect(page.locator('section.up').getByText('Actividad FIT (gzip)', { exact: true })).toBeVisible();
	await expect(page.getByRole('list', { name: 'lucia-ride.fit.gz' }).getByRole('listitem')).toHaveText(
		[
			'1 actividad leída',
			'20 minutos de frecuencia cardiaca importados',
			'0 distancias caminando o corriendo importadas',
			'1 total de energía de actividad importado',
			'1 distancia de otro deporte (ciclismo, natación…) sin guardar: la distancia de Wearables es caminando y corriendo',
			'Dispositivo: OwnHealth Test Watch'
		],
		{ timeout: 60_000 }
	);
	await page.screenshot({ path: 'test-results/fit-ride-es.png', fullPage: true });
});

test('a cut file and a course are refused with a reason in each language', async ({ page }) => {
	const cut = await upload(page, 'en', 'lucia-run-cut.fit');
	await expect(cut).toContainText('This FIT file is cut short, so it cannot be read.', { timeout: 60_000 });
	const course = await upload(page, 'es', 'lucia-course.fit');
	await expect(course).toContainText('Este archivo FIT no contiene ninguna actividad', { timeout: 60_000 });
	await page.screenshot({ path: 'test-results/fit-refused-es.png', fullPage: true });
	// no reason ever shows as the server's English
	await expect(page.locator('section.up')).not.toContainText('PermanentJobError');
	await expect(page.locator('section.up')).not.toContainText('holds no activity');
});
