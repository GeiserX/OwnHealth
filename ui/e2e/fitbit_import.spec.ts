import { expect, test } from '@playwright/test';

test('Fitbit Takeout imports both layouts and explains an unsupported ZIP in English and Spanish', async ({ page }) => {
	const files = ['legacy.zip', 'google-data.zip', 'unrecognized.zip'];
	for (const [lang, upload, label, readings, refusal] of [
		['en', 'Upload', 'Fitbit Takeout', '4 readings added', 'This ZIP has no supported Fitbit export files in a single account folder.'],
		['es', 'Subir', 'Exportación de Fitbit', 'Nada nuevo: las 4 mediciones ya estaban', 'Este ZIP no contiene archivos de Fitbit compatibles en una única carpeta de cuenta.']
	]) {
		await page.goto('/import-review');
		const language = page.getByRole('button', { name: lang.toUpperCase(), exact: true });
		await language.click();
		await expect(language).toHaveAttribute('aria-pressed', 'true');
		await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
		const open = page.getByRole('button', { name: upload, exact: true });
		if ((await open.getAttribute('aria-expanded')) !== 'true') await open.click();
		const panel = page.locator('section.up');
		await panel.locator('input[type=file]').first().setInputFiles(files.map((file) => `../fixtures/fitbit/${file}`));
		await expect(panel.getByText(readings, { exact: true })).toHaveCount(2, { timeout: 60_000 });
		await expect(panel.getByText(label, { exact: true })).toHaveCount(3);
		await expect(panel.getByText(refusal, { exact: true })).toBeVisible();
		for (const file of files.slice(0, 2)) {
			await expect(panel.getByRole('link', { name: file, exact: true })).toHaveAttribute('href', /^\?doc=/);
		}
		await expect(panel.getByRole('link', { name: files[2], exact: true })).toHaveCount(0);
		await panel.screenshot({ path: test.info().outputPath(`fitbit-${lang}.png`) });
		await panel.getByRole('link', { name: files[0], exact: true }).click();
		await expect(page).toHaveURL(/[?&]doc=/);
		await page.reload();
		await expect(page.getByText(label, { exact: true })).toBeVisible();
	}
});
