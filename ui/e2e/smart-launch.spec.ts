import { expect, test, type Page } from '@playwright/test';
import { execFileSync } from 'node:child_process';

const launcher = process.env.OWNHEALTH_E2E_SMART_LAUNCHER;
const fhir = process.env.OWNHEALTH_E2E_SMART_FHIR;
const db = process.env.OWNHEALTH_E2E_DB ?? '';
const app = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_PORT ?? 8216}`;
const patient = 'alex-rivera';
const label = 'Hospital LabNorte';
const observations = [
	{ id: 'labnorte-glucose', code: '2345-7', name: 'Glucose', value: 93, unit: 'mg/dL' },
	{ id: 'labnorte-creatinine', code: '2160-0', name: 'Creatinine', value: 0.91, unit: 'mg/dL' }
];

test.use({ baseURL: app, colorScheme: 'dark', trace: 'off' });
test.skip(!launcher, 'Start the pinned SMART launcher and its FHIR server for this spec.');

function sql(query: string): string {
	return execFileSync('docker', [
		'exec', 'ownhealth-testpg', 'psql', '-U', 'postgres', '-d', db, '-v', 'ON_ERROR_STOP=1', '-qAtc', query
	], { encoding: 'utf8' }).trim();
}

async function put(resourceType: string, id: string, fields: Record<string, unknown>) {
	const response = await fetch(`${fhir}/${resourceType}/${id}`, {
		method: 'PUT',
		headers: { 'Content-Type': 'application/fhir+json' },
		body: JSON.stringify({ resourceType, id, ...fields })
	});
	expect(response.ok, `seed ${resourceType}/${id}: HTTP ${response.status}`).toBe(true);
}

async function launch(page: Page, vendor: string) {
	expect(db, 'the app and spec share a fresh test database').toMatch(/^oh_e2e_[a-z0-9]+$/);
	expect(fhir, 'the launcher has a seeded FHIR upstream').toBeTruthy();
	const origin = new URL(launcher!).origin;
	const callback = `${app}/api/connectors/callback`;
	// The launcher codec supports this standalone patient configuration. Pick Alex in advance,
	// skip the simulated login, and keep both real consent and mandatory S256 validation.
	const sim = btoa(JSON.stringify({ k: '1', b: patient, i: '1', m: '1' })).replace(/=+$/, '');
	const issuer = `${origin}/v/r4/sim/${sim}/fhir`;
	const profile = sql("select id from profile where slug = 'alex'");
	expect(profile).toMatch(/^[0-9a-f-]{36}$/);
	// Client registration belongs to the connection wizard. Seed the public client and start
	// the flow through the same HTTP endpoint the wizard uses.
	const client = sql(`
		insert into connector_client (household_id, vendor, issuer, client_id, redirect_uri)
		select household_id, '${vendor}', '${issuer}', 'ownhealth-e2e', '${callback}'
		from profile where id = '${profile}' returning id
	`);

	await page.goto('/connections?lang=en');
	await expect(page.getByRole('heading', { name: 'Connections', level: 1 })).toBeVisible();
	const response = await page.request.post('/api/connectors/launch/smart', {
		headers: { origin: app },
		data: { profile_id: profile, client_id: client, label }
	});
	expect(response.status(), 'start the standalone launch').toBe(200);

	const authorize = new URL((await response.json()).authorize_url);
	expect(authorize.origin).toBe(origin);
	expect(authorize.pathname).toBe(`/v/r4/sim/${sim}/auth/authorize`);
	expect(authorize.searchParams.get('code_challenge_method')).toBe('S256');
	expect(authorize.searchParams.get('code_challenge')).toMatch(/^[A-Za-z0-9_-]{43}$/);
	const scopes = authorize.searchParams.get('scope')?.split(' ');
	expect(scopes).toContain('launch/patient');
	expect(scopes).toContain('offline_access');

	await page.goto(authorize.href).catch(() => {
		throw new Error('The SMART launcher did not open the consent page.');
	});
	await expect(page.getByRole('heading', { name: 'Authorize App Launch' })).toBeVisible();
	await expect(page.getByText('offline access', { exact: true })).toBeVisible();
	return { client, profile };
}

test('launches with PKCE and offline access, refreshes, and imports the patient record', async ({ page }) => {
	test.setTimeout(90_000);
	await put('Patient', patient, {
		name: [{ use: 'official', given: ['Alex'], family: 'Rivera' }],
		birthDate: '1979-05-14'
	});
	for (const observation of observations) {
		await put('Observation', observation.id, {
			status: 'final',
			category: [{ coding: [{ system: 'http://terminology.hl7.org/CodeSystem/observation-category', code: 'laboratory' }] }],
			code: { coding: [{ system: 'http://loinc.org', code: observation.code, display: observation.name }] },
			subject: { reference: `Patient/${patient}` },
			effectiveDateTime: '2026-10-19T08:00:00Z',
			valueQuantity: { value: observation.value, unit: observation.unit, system: 'http://unitsofmeasure.org', code: observation.unit }
		});
	}

	const { client, profile } = await launch(page, 'smart-launcher-positive');
	await page.getByRole('button', { name: 'Approve' }).click();
	await expect(page.getByRole('status').filter({ hasText: `${label} is connected.` })).toBeVisible();
	const source = sql(`select id from source where connector_client_id = '${client}' and profile_id = '${profile}' and kind = 'smart'`);
	expect(source).toMatch(/^[0-9a-f-]{36}$/);
	await expect.poll(() => sql(`
		select status from source_run where source_id = '${source}' order by id desc limit 1
	`), { timeout: 45_000 }).toBe('ok');

	const documentId = sql(`
		select d.id from document d
		join document_source ds on ds.document_id = d.id
		join source s on s.id = ds.source_id
		join secret k on k.id = s.secret_id
		where s.id = '${source}' and d.profile_id = '${profile}' and d.identity_match = 'matched'
		  and k.updated_at > k.created_at and k.updated_at <= d.created_at
	`);
	expect(documentId, 'OwnHealth refreshed the offline grant before storing the matched bundle').toMatch(/^[0-9a-f-]{36}$/);

	const file = await page.request.get(`/api/documents/${documentId}/file`);
	expect(file.status()).toBe(200);
	expect(await file.json()).toMatchObject({
		resourceType: 'Bundle',
		entry: expect.arrayContaining([
			{ resource: expect.objectContaining({ resourceType: 'Patient', id: patient, birthDate: '1979-05-14' }), fullUrl: expect.any(String) },
			...observations.map(({ id }) => expect.objectContaining({
				resource: expect.objectContaining({ resourceType: 'Observation', id, subject: { reference: `Patient/${patient}` } })
			}))
		])
	});
	expect(sql(`
		select loinc || '=' || trim_scale(value_num)
		from observation where document_id = '${documentId}' and profile_id = '${profile}' and status = 'current'
		order by loinc
	`).split('\n')).toEqual(observations.map(({ code, value }) => `${code}=${value}`).sort());

	const shots = process.env.OWNHEALTH_E2E_SMART_SHOTS;
	const card = page.getByRole('article', { name: label });
	await expect(card).toContainText('Alex Rivera');
	await expect(card).toHaveAttribute('data-state', 'ok');
	if (shots) await page.screenshot({ path: `${shots}/smart-en.png` });
	await page.goto('/connections?lang=es');
	await expect(page.getByRole('heading', { name: 'Conexiones', level: 1 })).toBeVisible();
	await expect(card).toHaveAttribute('data-state', 'ok');
	if (shots) await page.screenshot({ path: `${shots}/smart-es.png` });
});

test('rejects the authorization code with a wrong PKCE verifier', async ({ page }) => {
	const { client } = await launch(page, 'smart-launcher-negative');
	// Keep the browser's real code challenge and replace only the verifier OwnHealth will send.
	expect(sql(`
		update auth_challenge set data = jsonb_set(data, '{verifier}', to_jsonb(repeat('x', 64)))
		where purpose = 'connector' and data->>'client' = '${client}'
		returning 1
	`)).toBe('1');

	await page.getByRole('button', { name: 'Approve' }).click();
	const error = page.getByRole('alert').filter({ hasText: 'The vendor\'s server did not accept the sign-in.' });
	await expect(error).toHaveAttribute('data-code', 'exchange_failed');
	expect(sql(`select count(*) from source where connector_client_id = '${client}'`)).toBe('0');
});
