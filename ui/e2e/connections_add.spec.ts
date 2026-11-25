// The add-connection wizard (slice 19) against the fake vendors of fake-vendor.mjs: a person picks a
// SMART vendor, finds the facility in the US provider directory, copies the redirect URI and scopes,
// saves the app's client id and secret (sealed, never shown again), and connects; then Medicare (Blue
// Button) the same way, in Spanish. The directory is seeded with psql as a switched-off source, so
// opening the wizard never downloads a real vendor file. Every row made here is removed after.
import { expect, test, type Page } from '@playwright/test';
import { execFileSync } from 'node:child_process';

const db = process.env.OWNHEALTH_E2E_DB ?? '';
const app = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_PORT ?? 8190}`;
const fake = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_FAKE_PORT ?? 9190}`;

function sql(q: string): string {
	return execFileSync('docker', ['exec', 'ownhealth-testpg', 'psql', '-v', 'ON_ERROR_STOP=1', '-U', 'postgres', '-d', db, '-qAtc', q], { encoding: 'utf8' }).trim();
}

function collectPageErrors(page: Page): string[] {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	return errors;
}

/** A fresh client secret per run: no secret literal in the repository. */
const freshSecret = () => Array.from(crypto.getRandomValues(new Uint8Array(24)), (b) => b.toString(16).padStart(2, '0')).join('');

const CLIENT_IDS = ['e2e-healow', 'e2e-medicare'];
const SCOPES_V2 =
	'openid fhirUser launch/patient offline_access patient/Patient.rs patient/AllergyIntolerance.rs patient/Condition.rs patient/DiagnosticReport.rs patient/DocumentReference.rs patient/Encounter.rs patient/Immunization.rs patient/Medication.rs patient/MedicationRequest.rs patient/Observation.rs patient/Procedure.rs';

test.describe.configure({ mode: 'serial' });
test.use({ permissions: ['clipboard-read', 'clipboard-write'] });

let directory: string | null = null;

test.beforeAll(() => {
	expect(db, 'the config names the database').not.toBe('');
	// the household's directory, switched off so no run fetches the vendors' files: two facilities with
	// the same name at two vendors, so the search shows only the picked vendor's one
	const existing = sql("select s.id from source s join profile p on p.household_id = s.household_id where p.slug = 'alex' and s.kind = 'brands'");
	if (existing) return;
	directory = sql(
		"insert into source (household_id, kind, label, enabled) select household_id, 'brands', 'US provider directory', false from profile where slug = 'alex' returning id"
	);
	sql(`insert into fhir_brand_feed (source_id, vendor, fetched_at, entries) values ('${directory}', 'healow', now(), 1), ('${directory}', 'epic', now(), 1)`);
	sql(`insert into fhir_brand (source_id, vendor, name, endpoint, state) values
		('${directory}', 'healow', 'LabNorte Family Clinic', 'https://fhir4.healow.com/fhir/r4/LABNORTE', 'TX'),
		('${directory}', 'epic', 'LabNorte Family Clinic', 'https://fhir.labnorte.example/epic/r4', 'TX')`);
});

// Other specs share this database: remove only what this file made.
test.afterAll(() => {
	const ids = CLIENT_IDS.map((c) => `'${c}'`).join(',');
	sql(`begin;
		create temporary table wizard_e2e_client on commit drop as
			select id, client_secret_id from connector_client where client_id in (${ids});
		create temporary table wizard_e2e_source on commit drop as
			select id, secret_id from source where connector_client_id in (select id from wizard_e2e_client);
		delete from review_item where payload#>>'{source,id}' in (select id::text from wizard_e2e_source);
		delete from document where id in (
			select document_id from document_source where source_id in (select id from wizard_e2e_source)
		);
		delete from source where id in (select id from wizard_e2e_source);
		delete from secret where id in (select secret_id from wizard_e2e_source);
		delete from connector_client where id in (select id from wizard_e2e_client);
		commit`);
	if (directory) sql(`delete from source where id = '${directory}'`);
});

test('a person adds a SMART vendor from the directory, saves the client and connects, in English', async ({ page }) => {
	const errors = collectPageErrors(page);
	const secret = freshSecret();
	const alex = sql("select id from profile where slug = 'alex'");

	await page.goto('/connections?lang=en');
	await page.getByRole('link', { name: 'Add connection' }).click();
	await expect(page).toHaveURL(`${app}/connections/add`);
	await expect(page.getByRole('heading', { name: 'Add a connection', level: 1 })).toBeVisible();
	// every vendor of the file, none verified live
	const vendors = page.locator('.vd');
	await expect(vendors).toHaveCount(8);
	await expect(vendors.filter({ hasText: 'not verified live' })).toHaveCount(8);

	// a public vendor asks for no secret
	await page.getByText('Oracle Health', { exact: true }).click();
	await expect(page.locator('[data-vendor="oracle"]')).toContainText('Public app (client id only)');
	await expect(page.getByText('An access token lasts 570 seconds.')).toBeVisible();

	await page.getByText('eClinicalWorks (healow)', { exact: true }).click();
	await expect(page.getByText('No real account has connected through this vendor yet.')).toBeVisible();
	// opening a directory vendor kept the household's one directory, switched off
	expect(sql("select count(*) || ' ' || bool_or(enabled) from source s join profile p on p.household_id = s.household_id where p.slug = 'alex' and s.kind = 'brands'")).toBe('1 false');
	await page.getByLabel('Hospital or clinic name').fill('LabNorte');
	await page.getByRole('button', { name: 'Search' }).click();
	const hit = page.locator('.opt').filter({ hasText: 'LabNorte Family Clinic' });
	await expect(hit).toHaveCount(1); // the Epic facility of the same name is not this vendor's
	await expect(hit).toContainText('https://fhir4.healow.com/fhir/r4/LABNORTE');
	await hit.click();
	await expect(page.getByRole('status').filter({ hasText: 'Server: https://fhir4.healow.com/fhir/r4/LABNORTE' })).toBeVisible();

	// the registration steps: the redirect URI and the scopes, each with a copy button
	await expect(page.getByRole('link', { name: 'Registration page ↗' })).toHaveAttribute('href', 'https://connect4.healow.com/');
	await expect(page.getByTestId('cw-redirect')).toHaveText(`${app}/api/connectors/callback`);
	await expect(page.getByTestId('cw-scopes')).toHaveText(SCOPES_V2);
	const steps = page.locator('.steps');
	await steps.locator('li').nth(2).getByRole('button', { name: 'Copy' }).click();
	await expect(steps.locator('li').nth(2).getByRole('button', { name: 'Copied' })).toBeVisible();
	expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(`${app}/api/connectors/callback`);
	await steps.locator('li').nth(3).getByRole('button', { name: 'Copy' }).click();
	expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(SCOPES_V2);
	await expect(page.getByText('Each renewal brings a new refresh token; OwnHealth drops the old one.')).toBeVisible();

	// the facility's real server is not reachable from the test: the fake vendor's address is typed
	await page.getByText('Another server address', { exact: true }).click();
	await page.getByLabel('The FHIR base address the facility gives').fill(fake);
	await expect(page.getByRole('status').filter({ hasText: `Server: ${fake}` })).toBeVisible();

	// a confidential vendor needs the secret before it saves
	const save = page.getByRole('button', { name: 'Save client' });
	await page.getByLabel('Client id', { exact: true }).fill('e2e-healow');
	await expect(save).toBeDisabled();
	await page.getByLabel('Client secret', { exact: true }).fill(secret);
	const answer = page.waitForResponse((r) => r.url().endsWith('/api/connectors/clients') && r.request().method() === 'POST');
	await save.click();
	const saved = await answer;
	expect(saved.status()).toBe(201);
	expect(await saved.text()).not.toContain(secret);
	await expect(page.getByRole('status').filter({ hasText: 'Client saved. The secret is sealed and not shown again.' })).toBeVisible();
	await expect(page.getByText('A client is saved for this server: e2e-healow. Its secret is sealed.')).toBeVisible();
	// the secret is nowhere on the page after the save, and sealed in the database
	await expect(page.locator('input[type="password"]')).toHaveCount(0);
	expect(await page.content()).not.toContain(secret);
	expect(sql(`select count(*) from connector_client c join secret s on s.id = c.client_secret_id where c.client_id = 'e2e-healow' and c.issuer = '${fake}' and s.purpose = 'connector_client_secret'`)).toBe('1');
	expect(sql(`select count(*) from connector_client c, secret s where c::text like '%${secret}%' or s::text like '%${secret}%' or encode(s.ciphertext, 'escape') like '%${secret}%'`)).toBe('0');
	expect(sql(`select count(*) from audit_event a where a::text like '%${secret}%'`)).toBe('0');
	expect(sql("select count(*) from audit_event where args_summary like 'vendor=healow replaced=false%'")).toBe('1');

	// connect through the launch route: the fake consents and the callback lands on Connections
	await page.getByLabel('Whose record').selectOption(alex);
	await page.getByLabel('Name of the connection').fill('LabNorte Family Clinic');
	await page.getByRole('button', { name: 'Connect at eClinicalWorks (healow)' }).click();
	await expect(page.getByRole('status').filter({ hasText: 'LabNorte Family Clinic is connected.' })).toBeVisible();
	await expect(page).toHaveURL(`${app}/connections`);
	const card = page.getByRole('article', { name: 'LabNorte Family Clinic' });
	await expect(card).toContainText('Health record (SMART on FHIR)');
	await expect(card).toContainText(fake);
	expect(sql(`select count(*) from source s join connector_client c on c.id = s.connector_client_id where c.client_id = 'e2e-healow' and s.secret_id is not null`)).toBe('1');
	// the fake saw this client's first grant, so the sign-in shows as ending in 3 days
	expect(await (await page.request.get(`${fake}/__book?client=e2e-healow`)).json()).toMatchObject({ grants: 1 });
	await expect(card).toContainText('The sign-in at the vendor ends in 3 days.');
	expect(errors).toEqual([]);
});

test('Medicare through the wizard, in Spanish, with the client saved once and reused', async ({ page }) => {
	const errors = collectPageErrors(page);
	const secret = freshSecret();
	const alex = sql("select id from profile where slug = 'alex'");

	// Spanish for the whole visit, the vendor's return to Connections included
	await page.addInitScript(() => localStorage.setItem('oh.lang', 'es'));
	await page.goto('/connections/add');
	await expect(page.getByRole('heading', { name: 'Añadir una conexión', level: 1 })).toBeVisible();
	await expect(page.locator('.vd').filter({ hasText: 'sin verificar en real' })).toHaveCount(8);

	// the SMART client saved in English is found again for the same server
	await page.getByText('eClinicalWorks (healow)', { exact: true }).click();
	await page.getByText('Otra dirección de servidor', { exact: true }).click();
	await page.getByLabel('La dirección base FHIR que da el centro').fill(fake);
	await expect(page.getByText('Hay un cliente guardado para este servidor: e2e-healow. Su secreto está sellado.')).toBeVisible();
	await expect(page.getByRole('button', { name: 'Conectar en eClinicalWorks (healow)' })).toBeVisible();

	// Blue Button: the vendor's own servers, production first
	await page.getByText('CMS Blue Button 2.0', { exact: true }).click();
	await expect(page.getByRole('radio', { name: 'https://api.bluebutton.cms.gov/v3/fhir' })).toBeChecked();
	await expect(page.getByTestId('cw-scopes')).toHaveText('launch/patient patient/Patient.rs patient/Coverage.rs patient/ExplanationOfBenefit.rs');
	await expect(page.getByText('Cada token de renovación sirve una sola vez; OwnHealth guarda el nuevo al momento.')).toBeVisible();
	await expect(page.getByRole('link', { name: 'Página de registro ↗' })).toHaveAttribute('href', 'https://sandbox.bluebutton.cms.gov/');
	await page.getByText('Otra dirección de servidor', { exact: true }).click();
	await page.getByLabel('La dirección base FHIR que da el centro').fill(`${fake}/v3/fhir`);

	// a refusal reads in Spanish: a Blue Button address that is not a v3 base
	await page.getByLabel('La dirección base FHIR que da el centro').fill(`${fake}/v2/fhir`);
	await page.getByLabel('Id de cliente', { exact: true }).fill('e2e-medicare');
	await page.getByLabel('Secreto de cliente', { exact: true }).fill(secret);
	await page.getByRole('button', { name: 'Guardar cliente' }).click();
	await expect(page.getByRole('alert')).toHaveText('Medicare solo se conecta al servidor v3 de producción o de pruebas de CMS Blue Button.');
	// the secret left the page with the refused save
	await expect(page.getByLabel('Secreto de cliente', { exact: true })).toHaveValue('');
	expect(sql("select count(*) from connector_client where client_id = 'e2e-medicare'")).toBe('0');

	await page.getByLabel('La dirección base FHIR que da el centro').fill(`${fake}/v3/fhir`);
	await page.getByLabel('Secreto de cliente', { exact: true }).fill(secret);
	await page.getByRole('button', { name: 'Guardar cliente' }).click();
	await expect(page.getByRole('status').filter({ hasText: 'Cliente guardado. El secreto está sellado y no se vuelve a mostrar.' })).toBeVisible();
	expect(await page.content()).not.toContain(secret);

	await page.getByLabel('De quién es la historia').selectOption(alex);
	await page.getByLabel('Nombre de la conexión').fill('Medicare');
	await page.getByRole('button', { name: 'Conectar en CMS Blue Button 2.0' }).click();
	await expect(page.getByRole('status').filter({ hasText: 'Medicare está conectada.' })).toBeVisible();
	await expect(page).toHaveURL(`${app}/connections`);
	await expect(page.getByRole('article', { name: 'Medicare' })).toContainText('Medicare (Blue Button)');
	expect(errors).toEqual([]);
});
