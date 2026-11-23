// The Wearables screen says which vendor an HRV chart shows and which it left out (slice 25's summary
// note, rendered in slice 34), in each language. The page reads every provider, the ones the provider
// policy keeps from agents included (data/connectors/providers.yaml): an Oura ring shows here. The two
// sources are seeded with psql into the database serve.sh made, on the last days, for Lucía Rivera.
import { expect, test, type Page } from '@playwright/test';
import { execFileSync } from 'node:child_process';

const db = process.env.OWNHEALTH_E2E_DB ?? '';

// screenshots go to the run's own output folder (e2e/test-results), never into ui/
const shot = (page: Page, name: string) => page.screenshot({ path: test.info().outputPath(name), fullPage: true });

/** One statement in the test database; the first line of its answer. */
function sql(q: string): string {
	return execFileSync('docker', ['exec', 'ownhealth-testpg', 'psql', '-U', 'postgres', '-d', db, '-qAtc', q], { encoding: 'utf8' }).trim();
}

/** A wearable source of Lucía's with a daily reading on the given days before today. */
function rollup(provider: string, device: string, daysAgo: number[], v: number, metric = 'hrv_rmssd') {
	const pid = `(select id from profile where slug = 'lucia' order by created_at limit 1)`;
	sql(
		`insert into wearable_source (profile_id, provider, device) select ${pid}, '${provider}', '${device}' ` +
			`where not exists (select 1 from wearable_source where profile_id = ${pid} and provider = '${provider}' and device = '${device}')`
	);
	const src = `(select id from wearable_source where profile_id = ${pid} and provider = '${provider}' and device = '${device}')`;
	for (const d of daysAgo) {
		sql(
			`insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, v_p10, v_p50, v_p90, coverage) ` +
				`values (${pid}, '${metric}', ${src}, 'day', (current_date - ${d})::timestamp at time zone 'UTC', 1, ${v}, ${v}, ${v}, ${v}, ${v}, ${v}, ${v}, 1) ` +
				`on conflict do nothing`
		);
	}
}

test('an HRV chart names the vendor it shows and the one it left out, in English and Spanish', async ({ page }) => {
	expect(db, 'the config names the database').not.toBe('');
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	rollup('oura', 'Oura Ring', [2, 4, 6, 8], 48);
	rollup('gadgetbridge', 'Amazfit Band', [1, 3, 5, 7], 71);
	await page.goto('/wearables?lang=en');
	await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
	const card = page.locator('.card', { hasText: 'HRV (RMSSD)' });
	await expect(card.locator('.note')).toHaveText(
		'HRV from one source only: Amazfit Band. Not mixed with HRV from other sources in this window: Oura Ring.',
		{ timeout: 30_000 }
	);
	await shot(page, 'wearables-hrv-note-en.png');
	await page.goto('/wearables?lang=es');
	const tarjeta = page.locator('.card', { hasText: 'VFC (RMSSD)' });
	await expect(tarjeta.locator('.note')).toHaveText(
		'VFC de una sola fuente: Amazfit Band. No se mezcla con la VFC de otras fuentes en este periodo: Oura Ring.',
		{ timeout: 30_000 }
	);
	await shot(page, 'wearables-hrv-note-es.png');
	expect(errors).toEqual([]);
});

// The summary reads 90 days, and the best HRV source of those days may have no day in the 7 on screen.
// The page then reads those 7 days again: the chart and its note name the best source of the 7 days.
// The ring and the band as above (the seeds are idempotent), plus a watch with more, older days.
test('the HRV chart and its note follow the window on screen', async ({ page }) => {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	rollup('oura', 'Oura Ring', [2, 4, 6, 8], 48);
	rollup('gadgetbridge', 'Amazfit Band', [1, 3, 5, 7], 71);
	rollup('fit', 'Forerunner 265', Array.from({ length: 16 }, (_, i) => 10 + i), 55);
	await page.goto('/wearables?lang=en');
	await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
	const card = page.locator('.card', { hasText: 'HRV (RMSSD)' });
	// 30 days: the watch has the most days, so it is the one source
	await expect(page.getByRole('button', { name: '30 d', exact: true })).toHaveAttribute('aria-pressed', 'true', { timeout: 30_000 });
	await expect(card.locator('.note')).toHaveText(/^HRV from one source only: Forerunner 265\./, { timeout: 30_000 });
	await page.getByRole('button', { name: '7 d', exact: true }).click();
	// 7 days: the watch has none, so the band (newer than the ring) is drawn and named
	await expect(card.locator('.note')).toHaveText(
		'HRV from one source only: Amazfit Band. Not mixed with HRV from other sources in this window: Oura Ring.',
		{ timeout: 30_000 }
	);
	await expect(card.locator('.src')).toHaveText('Amazfit Band');
	await expect(card.getByText('No values yet.')).toHaveCount(0);
	await shot(page, 'wearables-hrv-7d-en.png');
	await page.getByRole('button', { name: '30 d', exact: true }).click();
	await expect(card.locator('.note')).toHaveText(/^HRV from one source only: Forerunner 265\./, { timeout: 30_000 });
	expect(errors).toEqual([]);
});

for (const [lang, hrvLabel, restLabel, failed, retry, empty] of [
	['en', 'HRV (RMSSD)', 'Resting heart rate', 'Could not load HRV for this period (HTTP 500 · internal).', 'Retry', 'No values yet.'],
	['es', 'VFC (RMSSD)', 'FC en reposo', 'No se pudo cargar la VFC de este periodo (HTTP 500 · internal).', 'Reintentar', 'Aún sin valores.'],
] as const) {
	test(`an HRV window failure can be retried in ${lang}`, async ({ page }) => {
		rollup('gadgetbridge', 'Amazfit Band', [1, 3, 5, 7], 71);
		rollup('fit', 'Forerunner 265', Array.from({ length: 16 }, (_, i) => 10 + i), 55);
		rollup('gadgetbridge', 'Amazfit Band', [1, 3, 5, 7], 60, 'hr_resting');
		let sevenDayReads = 0;
		await page.route(
			(url) => url.pathname === '/api/profiles/lucia/wearables/summary' && url.searchParams.get('days') === '7',
			async (route) => {
				if (++sevenDayReads === 1) {
					await route.fulfill({ status: 500, json: { detail: { code: 'internal', message: 'Internal error.' } } });
				} else {
					await route.continue();
				}
			}
		);
		await page.goto(`/wearables?lang=${lang}`);
		await page.getByRole('button', { name: 'Lucía Rivera', exact: true }).click();
		const card = page.locator('.card', { hasText: hrvLabel });
		const restCard = page.locator('.card', { hasText: restLabel });
		await expect(card.locator('.src')).toHaveText('Forerunner 265', { timeout: 30_000 });
		await page.getByRole('button', { name: '7 d', exact: true }).click();
		await expect(card.getByRole('status')).toContainText(failed);
		await expect(card.getByText(empty, { exact: true })).toHaveCount(0);
		await expect(card.locator('.src')).toHaveCount(0);
		await expect(restCard.locator('.src')).toHaveText('Amazfit Band');
		await card.getByRole('button', { name: retry, exact: true }).click();
		await expect(card.locator('.src')).toHaveText('Amazfit Band');
		await expect(card.getByRole('status')).toHaveCount(0);
		expect(sevenDayReads).toBe(2);
	});
}
