import { expect, test } from '@playwright/test';

test('Strava history shows imported and unmatched files in English and Spanish', async ({ page }) => {
	for (const [lang, upload, title, count, missing, unlisted, omitted, invalid, gps] of [
		['en', 'Upload', 'Strava export', '2 files imported', '2 activity rows without a file', '1 activity file without a CSV row', '1 entry left out', 'Gzip could not be read', 'GPS routes were left out.'],
		['es', 'Subir', 'Exportación de Strava', '2 archivos importados', '2 filas de actividad sin archivo', '1 archivo de actividad sin fila en el CSV', '1 elemento sin importar', 'No se pudo leer el archivo gzip', 'Las rutas GPS se dejaron fuera.']
	]) {
		await page.goto('/import-review');
		const language = page.getByRole('button', { name: lang.toUpperCase(), exact: true });
		await language.click();
		await expect(language).toHaveAttribute('aria-pressed', 'true');
		await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
		const open = page.getByRole('button', { name: upload, exact: true });
		if ((await open.getAttribute('aria-expanded')) !== 'true') await open.click();
		const panel = page.locator('section.up');
		await panel.locator('input[type=file]').first().setInputFiles('../fixtures/strava/lucia-history.zip');
		await expect(panel.getByText(count, { exact: true })).toBeVisible({ timeout: 60_000 });
		for (const text of [title, missing, unlisted, gps]) {
			await expect(panel.getByText(text, { exact: true })).toBeVisible();
		}
		await panel.getByText(omitted, { exact: true }).click();
		await expect(panel.getByRole('listitem').filter({ hasText: 'activities/5.tcx.gz' })).toHaveText(
			'activities/5.tcx.gz · ' + invalid
		);
		await page.screenshot({ path: test.info().outputPath('strava-' + lang + '.png'), fullPage: true });
		await panel.getByRole('link', { name: 'lucia-history.zip', exact: true }).click();
		await expect(page).toHaveURL(/[?&]doc=/);
		await page.reload();
		await expect(page.locator('section.up')).toHaveCount(0);
		await expect(page.getByText(missing, { exact: true })).toBeVisible();
	}
});
