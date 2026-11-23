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

/** A wearable source of Lucía's with a daily rMSSD on the given days before today. */
function hrv(provider: string, device: string, daysAgo: number[], v: number) {
	const pid = `(select id from profile where slug = 'lucia' order by created_at limit 1)`;
	sql(
		`insert into wearable_source (profile_id, provider, device) select ${pid}, '${provider}', '${device}' ` +
			`where not exists (select 1 from wearable_source where profile_id = ${pid} and provider = '${provider}' and device = '${device}')`
	);
	const src = `(select id from wearable_source where profile_id = ${pid} and provider = '${provider}' and device = '${device}')`;
	for (const d of daysAgo) {
		sql(
			`insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, v_p10, v_p50, v_p90, coverage) ` +
				`values (${pid}, 'hrv_rmssd', ${src}, 'day', (current_date - ${d})::timestamp at time zone 'UTC', 1, ${v}, ${v}, ${v}, ${v}, ${v}, ${v}, ${v}, 1) ` +
				`on conflict do nothing`
		);
	}
}

test('an HRV chart names the vendor it shows and the one it left out, in English and Spanish', async ({ page }) => {
	expect(db, 'the config names the database').not.toBe('');
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	hrv('oura', 'Oura Ring', [2, 4, 6, 8], 48);
	hrv('gadgetbridge', 'Amazfit Band', [1, 3, 5, 7], 71);
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
