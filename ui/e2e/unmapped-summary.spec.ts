// The review page shows the one item a connector pull files per resource type the app does not show
// yet (fhir_unmapped_summary), in en and es: the connection, the count and the type, a link to the
// pulled bundle through the document route the profile's queue already uses, and Dismiss. No raw key
// shows. The FHIR tile names the pull's bundle, before and after its items are handled. The poller is
// slice 15, so the bundle is uploaded, its import job is removed (a pull runs none), and the item is
// seeded with psql into the database serve.sh made, as the importer writes it.
import { expect, test, type Page } from '@playwright/test';
import { execFileSync } from 'node:child_process';

const db = process.env.OWNHEALTH_E2E_DB ?? '';

/** One statement in the test database; the first line of its answer. */
function sql(q: string): string {
	return execFileSync('docker', ['exec', 'ownhealth-testpg', 'psql', '-U', 'postgres', '-d', db, '-qAtc', q], { encoding: 'utf8' }).trim();
}

function collectPageErrors(page: Page): string[] {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	return errors;
}

/** What `fhir::import_pull` files for one type of one pull. */
function seedSummary(doc: string, type: string, count: number): string {
	const payload = JSON.stringify({
		projection: null,
		resource_type: type,
		count,
		notes: { 'stored, not projected': count },
		source: { id: '00000000-0000-4000-8000-000000000014', kind: 'smart', label: 'Hospital LabNorte' },
		import_refs: [`pull-1/${type}/1`]
	}).replaceAll("'", "''");
	return sql(
		`insert into review_item (household_id, profile_id, kind, document_id, reasons, payload) select household_id, id, 'fhir_unmapped_summary', '${doc}', '{unmapped}', '${payload}' from profile where slug = 'alex' returning id`
	);
}

test('a pull’s grouped item reads in en and es, opens its bundle and is dismissed', async ({ page }) => {
	const errors = collectPageErrors(page);
	expect(db, 'the config names the database').not.toBe('');
	await page.goto('/import-review?lang=en');
	await page.evaluate(() => localStorage.setItem('oh.profile', 'alex'));

	// the pulled bundle, stored as one of Alex's documents (its Patient alone queues nothing)
	const bundle = { resourceType: 'Bundle', id: 'pull-1', type: 'searchset', entry: [{ resource: { resourceType: 'Patient', id: 'alex-rivera', name: [{ family: 'Rivera', given: ['Alex'] }], birthDate: '1979-05-14' } }] };
	const sha = await page.evaluate(async (text) => {
		const form = new FormData();
		form.set('profile', 'alex');
		form.append('files', new Blob([text], { type: 'application/fhir+json' }), 'Hospital LabNorte · pull.json');
		const r = await fetch('/api/imports', { method: 'POST', body: form, headers: { accept: 'application/json' } });
		const body = await r.text();
		return (JSON.parse(body) as { items?: { sha256?: string }[] }).items?.[0]?.sha256 ?? `${r.status} ${body}`;
	}, JSON.stringify(bundle));
	expect(sha).toMatch(/^[0-9a-f]{64}$/);
	// the import job files the document
	const docOf = () => sql(`select d.id from document d join profile p on p.id = d.profile_id where p.slug = 'alex' and d.blob_sha256 = decode('${sha}', 'hex')`);
	await expect.poll(docOf, { timeout: 20_000 }).toMatch(/^[0-9a-f-]{36}$/);
	const doc = docOf();
	// a connector pull runs no import job: once it is done, drop it, so only the document names the import
	const jobOf = `select state from job where kind = 'import.fhir_bundle' and payload->>'blob_sha256' = '${sha}'`;
	await expect.poll(() => sql(jobOf), { timeout: 20_000 }).toBe('done');
	sql(`delete from job where kind = 'import.fhir_bundle' and payload->>'blob_sha256' = '${sha}'`);
	expect(sql(jobOf)).toBe('');
	const claims = seedSummary(doc, 'ExplanationOfBenefit', 12000);
	const cover = seedSummary(doc, 'Coverage', 1);

	await page.reload();
	const row = page.locator(`#oi-${claims.slice(0, 8)}`);
	await expect(row).toContainText('Stored resources without a mapping');
	// the count as the reader's language writes numbers
	await expect(row).toContainText('Hospital LabNorte: 12,000 ExplanationOfBenefit resources stored as received');
	await expect(row).toContainText('so they are not listed one by one');
	await expect(page.locator(`#oi-${cover.slice(0, 8)}`)).toContainText('Hospital LabNorte: 1 Coverage resource stored as received');
	await expect(page.locator('body')).not.toContainText(/i2_\w+/);
	// the FHIR tile counts the grouped items with the per-resource ones
	const fhirOpen = sql(
		`select count(*) from review_item r join profile p on p.id = r.profile_id where p.slug = 'alex' and r.status = 'open' and r.kind in ('fhir_unmapped', 'fhir_unmapped_summary')`
	);
	const tile = page.locator('.ib', { hasText: 'FHIR / IPS' });
	await expect(tile.locator('.c')).toHaveText(fhirOpen);
	expect(Number(fhirOpen)).toBeGreaterThanOrEqual(2);
	// with no import job, the tile names the newest FHIR document: the pull's bundle
	await expect(tile).toContainText('Hospital LabNorte · pull.json');

	// the bundle opens through the document route, as the person who sees this queue
	const open = row.getByRole('link', { name: 'Open the downloaded file' });
	await expect(open).toHaveAttribute('href', `/api/documents/${doc}/file`);
	const file = await page.request.get(`/api/documents/${doc}/file`);
	expect(file.status()).toBe(200);
	expect(((await file.json()) as { id: string }).id).toBe('pull-1');
	// review accepts none of these: the row offers no Accept
	await expect(row.getByRole('button', { name: 'Accept' })).toHaveCount(0);

	await page.goto('/import-review?lang=es');
	await expect(row).toContainText('Recursos guardados sin correspondencia');
	await expect(row).toContainText('Hospital LabNorte: 12.000 recursos ExplanationOfBenefit guardados tal como llegaron');
	await expect(row).toContainText('El archivo descargado los guarda todos.');
	await expect(row.getByRole('link', { name: 'Abrir el archivo descargado' })).toBeVisible();
	await expect(page.locator(`#oi-${cover.slice(0, 8)}`)).toContainText('1 recurso Coverage guardado tal como llegó');
	await expect(page.locator('body')).not.toContainText(/i2_\w+/);

	await row.getByRole('button', { name: 'Descartar' }).click();
	await expect(row).toHaveCount(0);
	expect(sql(`select status from review_item where id = '${claims}'`)).toBe('dismissed');
	await page.locator(`#oi-${cover.slice(0, 8)}`).getByRole('button', { name: 'Descartar' }).click();
	await expect(page.locator(`#oi-${cover.slice(0, 8)}`)).toHaveCount(0);
	// every item of the pull handled: the tile still names its bundle, never "nothing imported yet"
	await page.reload();
	await expect(tile).toContainText('Hospital LabNorte · pull.json');
	await expect(tile).not.toContainText('nada importado aún');
	expect(errors).toEqual([]);
});
