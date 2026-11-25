import { expect, test } from '@playwright/test';

test('Garmin history lists imported and omitted files in both languages', async ({ page }) => {
	// the second upload is the same file again: it says that nothing new came in
	for (const [lang, upload, summary, count, readings, omitted, why, gps] of [
		['en', 'Upload', 'Garmin export', '2 files imported', '34 readings added', '1 entry left out', 'File type is not supported', 'GPS routes were left out.'],
		['es', 'Subir', 'Exportación de Garmin', '2 archivos importados', 'Nada nuevo: las 34 mediciones ya estaban', '1 elemento sin importar', 'Tipo de archivo no compatible', 'Las rutas GPS se dejaron fuera.']
	]) {
		await page.goto('/import-review');
		const language = page.getByRole('button', { name: lang.toUpperCase(), exact: true });
		await language.click();
		await expect(language).toHaveAttribute('aria-pressed', 'true');
		await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
		const open = page.getByRole('button', { name: upload, exact: true });
		if ((await open.getAttribute('aria-expanded')) !== 'true') await open.click();
		const panel = page.locator('section.up');
		await panel.locator('input[type=file]').first().setInputFiles('../fixtures/garmin/lucia-history.zip');
		await expect(panel.getByText(count, { exact: true })).toBeVisible({ timeout: 60_000 });
		await expect(panel.getByText(summary, { exact: true })).toBeVisible();
		await expect(panel.getByText(readings, { exact: true })).toBeVisible();
		await expect(panel.getByText(gps, { exact: true })).toBeVisible();
		await panel.getByText(omitted, { exact: true }).click();
		await expect(panel.getByText('DI_CONNECT/DI-Connect-Wellness/123_userSettings.json', { exact: true })).toBeVisible();
		await expect(panel.getByRole('listitem').filter({ hasText: '123_userSettings.json' })).toHaveText(`DI_CONNECT/DI-Connect-Wellness/123_userSettings.json · ${why}`);
		await page.screenshot({ path: test.info().outputPath(`garmin-${lang}.png`), fullPage: true });
		await panel.getByRole('link', { name: 'lucia-history.zip', exact: true }).click();
		await expect(page).toHaveURL(/[?&]doc=/);
		await page.reload();
		await expect(page.locator('section.up')).toHaveCount(0);
		await expect(page.getByText(count, { exact: true })).toBeVisible();
	}
});
