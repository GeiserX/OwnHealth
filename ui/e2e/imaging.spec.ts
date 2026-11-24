// Imaging (slice 42) against the fake Orthanc (fake-orthanc.mjs): a person finds Alex's study on the
// Imaging page and on the timeline, with the report the import linked to it, and opens it in the viewer,
// which runs sandboxed; a study the image store does not hold as recorded is refused in words, in es and
// en; a study whose disc gives no date is still listed. Then /import-review keeps keyboard focus on the
// queue after a Dismiss, with or without a confirm step. The study rows (the DICOM
// import's output, slice 41) and the held studies are seeded with psql into the database serve.sh made.
import { expect, test, type Page } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';

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

test.describe.configure({ mode: 'serial' });

// fake-orthanc.mjs holds one study under every UID below this root, as `fake-<uid>`
const run = randomBytes(3).toString('hex');
const knee = `1.2.826.0.1.3680043.10.4242.${parseInt(run, 16)}.1`;
const moved = `1.2.826.0.1.3680043.10.4343.${parseInt(run, 16)}.2`; // recorded in Orthanc, which no longer holds it
let report = '';

/** A study on Alex's record as the DICOM import writes it. */
function study(uid: string, orthancId: string | null, modality: string, description: string, day: string | null, reportId: string | null) {
	const src = `src_e2e42_${randomBytes(4).toString('hex')}`;
	sql(
		`insert into src (id, profile_id, kind, import_ref, label, actor_kind) select '${src}', id, 'dicom', 'imaging_import/e2e#${uid}', 'CD_Rivera.zip', 'system' from profile where slug = 'alex'`
	);
	sql(
		`insert into imaging_study (profile_id, study_instance_uid, orthanc_id, modality, study_date, description, report_document_id, status, src_id, fhir) select id, '${uid}', ${orthancId ? `'${orthancId}'` : 'null'}, '${modality}', ${day ? `'${day}'` : 'null'}, '${description}', ${reportId ? `'${reportId}'` : 'null'}, 'current', '${src}', '{"resourceType": "ImagingStudy", "numberOfSeries": 2, "numberOfInstances": 6}' from profile where slug = 'alex'`
	);
}

test('a person opens a study from the Imaging page and from the timeline, with its report beside it', async ({ page }) => {
	const errors = collectPageErrors(page);
	expect(db, 'the config names the database').not.toBe('');
	// no study yet: the page says so once
	await page.goto('/imaging?lang=en');
	await expect(page.getByText('No studies yet.', { exact: false })).toHaveCount(1);
	report = sql(
		`insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type, provider_name, collected_at) select household_id, id, decode('${randomBytes(32).toString('hex')}', 'hex'), 'Informe_RM_rodilla_e2e.pdf', 'application/pdf', 10, 'imaging_report', 'LabNorte Imagen', '2026-09-14 10:00+00' from profile where slug = 'alex' returning id`
	);
	study(knee, `fake-${knee}`, 'MR', 'MRI right knee', '2026-09-14', report);
	study(moved, `fake-${moved}`, 'CR', 'Chest X-ray', '2025-01-02', null);

	await page.goto('/imaging?lang=en');
	await expect(page.getByRole('heading', { name: 'Imaging', level: 1 })).toBeVisible();
	// the placeholder of 0.4 is gone
	await expect(page.getByText('Imaging arrives in v1.0')).toHaveCount(0);
	const kneeRow = page.getByRole('button', { name: /MRI right knee/ });
	await expect(kneeRow).toHaveAttribute('aria-pressed', 'true'); // the newest study is picked
	await expect(page.getByRole('cell', { name: knee })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Open the report (PDF)' })).toHaveAttribute('href', `/api/documents/${report}/file`);
	await page.screenshot({ path: test.info().outputPath('imaging-en.png'), fullPage: true });

	// Open: a new tab with the viewer of this one study, in an opaque origin
	const [viewer] = await Promise.all([page.waitForEvent('popup'), page.getByRole('button', { name: 'Open in OHIF ↗' }).click()]);
	await viewer.waitForURL(new RegExp(`/ohif/t/[A-Za-z0-9_-]{43}/viewer\\?StudyInstanceUIDs=${knee.replaceAll('.', '\\.')}$`));
	await expect(viewer.locator('#fake-ohif')).toHaveText('Fake OHIF: viewer');
	expect(await viewer.evaluate(() => window.origin)).toBe('null');
	await viewer.close();

	// a study the image store does not hold as recorded: the tab closes again and the page says why
	await page.getByRole('button', { name: /Chest X-ray/ }).click();
	const [refused] = await Promise.all([page.waitForEvent('popup'), page.getByRole('button', { name: 'Open in OHIF ↗' }).click()]);
	await expect(page.getByRole('alert')).toHaveText(/The image store does not hold this study as it was recorded/);
	await expect.poll(() => refused.isClosed()).toBe(true);
	await expect(page.getByText('No report came with this study.')).toBeVisible();

	// The timeline: the study is one event, its report is a link in it and no document event of its own
	await page.goto('/timeline?lang=en');
	const event = page.locator('.ev', { hasText: 'MRI right knee' });
	await expect(event.getByRole('link', { name: 'Open the report (PDF)' })).toHaveAttribute('href', `/api/documents/${report}/file`);
	await expect(page.getByText('Informe_RM_rodilla_e2e.pdf')).toHaveCount(0);
	await page.screenshot({ path: test.info().outputPath('imaging-timeline-en.png'), fullPage: true });
	const [fromTimeline] = await Promise.all([page.waitForEvent('popup'), event.getByRole('button', { name: 'Open in OHIF ↗' }).click()]);
	await fromTimeline.waitForURL(/\/ohif\/t\/[A-Za-z0-9_-]{43}\/viewer\?StudyInstanceUIDs=/);
	await expect(fromTimeline.locator('#fake-ohif')).toHaveText('Fake OHIF: viewer');
	await fromTimeline.close();
	expect(errors).toEqual([]);
});

test('the Imaging page reads in Spanish, refusals included', async ({ page }) => {
	await page.goto('/imaging?lang=es');
	await expect(page.getByRole('heading', { name: 'Imagen médica', level: 1 })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Abrir el informe (PDF)' })).toBeVisible();
	await expect(page.getByRole('button', { name: 'Importar carpeta DICOM / CD' })).toBeVisible();
	await page.screenshot({ path: test.info().outputPath('imaging-es.png'), fullPage: true });
	await page.getByRole('button', { name: /Chest X-ray/ }).click();
	const [refused] = await Promise.all([page.waitForEvent('popup'), page.getByRole('button', { name: 'Abrir en OHIF ↗' }).click()]);
	await expect(page.getByRole('alert')).toHaveText(/El almacén de imágenes no guarda este estudio tal como se registró/);
	await expect.poll(() => refused.isClosed()).toBe(true);
});

test('a study whose disc gives no date is listed on its report day, and says so', async ({ page }) => {
	const undated = `1.2.826.0.1.3680043.10.4545.${parseInt(run, 16)}.3`;
	const ct = sql(
		`insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type, provider_name, collected_at) select household_id, id, decode('${randomBytes(32).toString('hex')}', 'hex'), 'Informe_TC_e2e.pdf', 'application/pdf', 10, 'imaging_report', 'LabNorte Imagen', '2026-08-03 10:00+00' from profile where slug = 'alex' returning id`
	);
	study(undated, `fake-${undated}`, 'CT', 'CT abdomen', null, ct);
	await page.goto('/imaging?lang=en');
	const row = page.getByRole('button', { name: /CT abdomen/ });
	await expect(row).toContainText('2026-08-03 (the disc gives no date)');
	await row.click();
	await expect(page.getByRole('cell', { name: undated })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Open the report (PDF)' })).toHaveAttribute('href', `/api/documents/${ct}/file`);
	await page.goto('/imaging?lang=es');
	await expect(page.getByRole('button', { name: /CT abdomen/ })).toContainText('2026-08-03 (el disco no trae fecha)');
	sql(`update imaging_study set status = 'retracted', status_reason = 'e2e' where study_instance_uid = '${undated}'`);
});

test('after a Dismiss on /import-review, focus moves to the next row, not to the page', async ({ page }) => {
	// two held studies the person may only dismiss (another record holds their UID)
	const held = [1, 2].map((n) =>
		sql(
			`insert into review_item (household_id, profile_id, kind, reasons, payload) select household_id, id, 'imaging_study', '{other_profile}', '{"reason": "other_profile", "study_uid": "1.2.826.0.1.3680043.10.4444.${n}", "description": "Held study ${run} ${n}", "modality": "CT", "study_date": "2026-0${n}-01"}' from profile where slug = 'alex' returning id`
		)
	);
	await page.goto('/import-review?lang=en');
	const first = page.locator(`[data-row="${held[0]}"]`);
	await expect(first).toBeVisible();
	const order = await page.evaluate(() => [...document.querySelectorAll<HTMLElement>('[data-row]')].map((r) => r.dataset.row));
	const next = order[order.indexOf(held[0]) + 1] ?? order[order.indexOf(held[0]) - 1];
	await first.getByRole('button', { name: 'Dismiss' }).focus();
	await page.keyboard.press('Enter');
	await expect(first).toHaveCount(0);
	await expect
		.poll(() => page.evaluate(() => (document.activeElement as HTMLElement | null)?.closest<HTMLElement>('[data-row]')?.dataset.row ?? document.activeElement?.tagName))
		.toBe(next);
	// the keyboard carries on in the queue: Enter dismisses the row that now has focus
	if (next === held[1]) {
		await page.keyboard.press('Enter');
		await expect(page.locator(`[data-row="${held[1]}"]`)).toHaveCount(0);
	}
	sql(`update review_item set status = 'dismissed' where id in ('${held[0]}', '${held[1]}') and status = 'open'`);
});

test('after a Dismiss with a confirm step on /import-review, focus moves to the next row too', async ({ page }) => {
	// a scan waiting for its values (pending_document) asks to confirm a Dismiss in a row of its own
	const scans = [1, 2].map((n) => {
		const doc = sql(
			`insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes) select household_id, id, decode('${randomBytes(32).toString('hex')}', 'hex'), 'scan_${run}_${n}.pdf', 'application/pdf', 10 from profile where slug = 'alex' returning id`
		);
		return sql(
			`insert into review_item (household_id, profile_id, kind, document_id, payload) select household_id, id, 'pending_document', '${doc}', '{"why": "no_text", "pages": [1]}' from profile where slug = 'alex' returning id`
		);
	});
	await page.goto('/import-review?lang=en');
	const first = page.locator(`[data-row="${scans[0]}"]`);
	await expect(first).toBeVisible();
	const order = await page.evaluate(() => [...document.querySelectorAll<HTMLElement>('[data-row]')].map((r) => r.dataset.row));
	const next = order[order.indexOf(scans[0]) + 1] ?? order[order.indexOf(scans[0]) - 1];
	await first.getByRole('button', { name: 'Dismiss' }).focus();
	await page.keyboard.press('Enter');
	const confirm = page.locator('tr.sub').getByRole('button', { name: 'Dismiss' });
	await confirm.focus();
	await page.keyboard.press('Enter');
	await expect(first).toHaveCount(0);
	await expect
		.poll(() => page.evaluate(() => (document.activeElement as HTMLElement | null)?.closest<HTMLElement>('[data-row]')?.dataset.row ?? document.activeElement?.tagName))
		.toBe(next);
	sql(`update review_item set status = 'dismissed' where id in ('${scans[0]}', '${scans[1]}') and status = 'open'`);
});

// Import polling races, with the import and job answers mocked so their order is fixed.
const job = (id: number, state: string, result: Record<string, unknown> | null = null) => ({
	id,
	kind: 'import.dicom',
	state,
	attempts: 1,
	max_attempts: 3,
	last_error: null,
	result,
	created_at: '2026-11-24T10:00:00Z',
	finished_at: state === 'done' ? '2026-11-24T10:00:05Z' : null,
	profile_id: null
});
// two tiny files the mocked import never reads
const discDir = `${tmpdir()}/oh-e2e-imaging-${run}`;
mkdirSync(discDir, { recursive: true });
const disc = `${discDir}/disc.zip`;
const disc2 = `${discDir}/disc2.zip`;
writeFileSync(disc, 'PK');
writeFileSync(disc2, 'PK');

test('an upload that answers after the person left the Imaging page starts no polling', async ({ page }) => {
	let release: () => void = () => undefined;
	const held = new Promise<void>((r) => (release = r));
	await page.route((u) => u.pathname === '/api/imaging/import', async (route) => {
		await held;
		await route.fulfill({ json: { files: 1, duplicate: false, job_id: 424201 } });
	});
	let polls = 0;
	// other parts of the app read /api/jobs too; only this upload's job counts
	await page.route(
		(u) => u.pathname === '/api/jobs' && u.searchParams.get('ids') === '424201',
		async (route) => {
			polls += 1;
			await route.fulfill({ json: { jobs: [job(424201, 'running')] } });
		}
	);
	await page.goto('/imaging?lang=en');
	await page.locator('input[type=file][accept]').setInputFiles(disc);
	await expect(page.getByRole('status')).toContainText('Uploading 1 file');
	await page.locator('a[href="/timeline"]').first().click();
	await expect(page).toHaveURL(/\/timeline/);
	release();
	await page.waitForTimeout(5000); // two and a half poll periods
	expect(polls).toBe(0);
});

test('a poll answer for a replaced upload never shows on the newer one', async ({ page }) => {
	let uploads = 0;
	await page.route((u) => u.pathname === '/api/imaging/import', async (route) => {
		uploads += 1;
		await route.fulfill({ json: { files: uploads === 1 ? 1 : 2, duplicate: false, job_id: 424210 + uploads } });
	});
	let releaseFirst: () => void = () => undefined;
	const firstHeld = new Promise<void>((r) => (releaseFirst = r));
	let firstAsked: () => void = () => undefined;
	const firstSeen = new Promise<void>((r) => (firstAsked = r));
	await page.route((u) => u.pathname === '/api/jobs', async (route) => {
		const ids = new URL(route.request().url()).searchParams.get('ids');
		if (ids === '424211') {
			firstAsked();
			await firstHeld;
			await route.fulfill({ json: { jobs: [job(424211, 'done', { imported: 7 })] } });
		} else await new Promise(() => undefined); // the newer upload's poll never answers, so nothing overwrites a stale write
	});
	await page.goto('/imaging?lang=en');
	const zip = page.locator('input[type=file][accept]');
	await zip.setInputFiles(disc);
	await firstSeen; // the first upload's poll is on its way
	await zip.setInputFiles(disc2);
	await expect(page.getByRole('status')).toContainText('Upload kept: 2 files');
	releaseFirst();
	await page.waitForTimeout(1500);
	// read once, after the stale answer had time to land: a retrying assertion could wait it out
	const banner = await page.getByRole('status').innerText();
	expect(banner).toContain('Upload kept: 2 files');
	expect(banner).not.toContain('7');
});

