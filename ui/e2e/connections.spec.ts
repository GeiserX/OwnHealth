// The Connections screen against the fake SMART vendor (fake-vendor.mjs): a person connects Alex's
// record, sees the sign-in end soon, sees the vendor ask for consent again, reconnects (the reverse
// path clears it), follows an open sync-gap alert to review, reads every callback error in es and
// en, and disconnects. Rows the screen cannot create yet (the vendor client of slice 19, the flag a
// refused refresh sets, a gap alert of slice 37) are seeded with psql into the database serve.sh made.
import { expect, test, type Page } from '@playwright/test';
import { execFileSync } from 'node:child_process';

const db = process.env.OWNHEALTH_E2E_DB ?? '';
const app = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_PORT ?? 8190}`;
const fake = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_FAKE_PORT ?? 9190}`;

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

let source = '';

test('a person connects a record, sees a short grant, reconnects after the vendor refused it, and disconnects', async ({ page }) => {
	const errors = collectPageErrors(page);
	expect(db, 'the config names the database').not.toBe('');
	const profile = sql("select id from profile where slug = 'alex'");
	const client = sql(
		`insert into connector_client (household_id, vendor, issuer, client_id, redirect_uri) select household_id, 'labnorte-ehr', '${fake}', 'e2e-client', '${app}/api/connectors/callback' from profile where id = '${profile}' returning id`
	);

	await page.goto('/connections?lang=en');
	await expect(page.getByRole('heading', { name: 'Connections', level: 1 })).toBeVisible();
	await expect(page.getByText('No connections yet.')).toBeVisible();

	// The launch is the add wizard's (slice 19); here the page calls the route the wizard will call.
	const authorize = await page.evaluate(
		async ({ profile, client }) => {
			const r = await fetch('/api/connectors/launch/smart', {
				method: 'POST',
				headers: { 'content-type': 'application/json', accept: 'application/json' },
				body: JSON.stringify({ profile_id: profile, client_id: client, label: 'Hospital LabNorte' })
			});
			return ((await r.json()) as { authorize_url: string }).authorize_url;
		},
		{ profile, client }
	);
	expect(authorize.startsWith(`${fake}/authorize?`), authorize).toBe(true);
	await page.goto(authorize); // the fake consents and sends the browser to the callback, then here
	await expect(page.getByRole('status').filter({ hasText: 'Hospital LabNorte is connected.' })).toBeVisible();
	await expect(page).toHaveURL(`${app}/connections`); // the address no longer repeats the outcome
	source = sql("select id from source where kind = 'smart'");
	// the first daily poll ran (the SMART poller is slice 15): before it, a connection has no data yet
	sql(`update source set last_run_at = now(), last_success_at = now() where id = '${source}'`);

	const card = page.getByRole('article', { name: 'Hospital LabNorte' });
	// a grant of 3 days shows before it ends
	await expect(card).toHaveAttribute('data-state', 'grant_soon');
	await expect(card.locator('.pill')).toHaveText('Watch');
	await expect(card).toContainText('The sign-in at the vendor ends in 3 days.');
	await expect(card).toContainText('Health record (SMART on FHIR)');
	await expect(card).toContainText(fake);
	await expect(card).toContainText('Alex Rivera');
	await expect(card).toContainText('renewed by itself');

	// the vendor refused the refresh token: the screen asks for a new sign-in
	sql(`update source set reconsent_needed_at = now() where id = '${source}'`);
	await page.reload();
	await expect(card).toHaveAttribute('data-state', 'reconsent');
	await expect(card.locator('.pill')).toHaveText('Out');
	await expect(card).toContainText('The vendor no longer accepts this connection.');
	await expect(page.locator('.tiles .ib').nth(1).locator('.c')).toHaveText('1');

	// the reverse path: Reconnect, the vendor, the callback, and the flag is gone
	await card.getByRole('button', { name: 'Reconnect' }).click();
	await expect(page.getByRole('status').filter({ hasText: 'Hospital LabNorte is connected.' })).toBeVisible();
	await expect(card).toHaveAttribute('data-state', 'ok');
	await expect(card.locator('.pill')).toHaveText('OK');
	await expect(card).not.toContainText('no longer accepts');
	expect(sql(`select reconsent_needed_at is null from source where id = '${source}'`)).toBe('t');
	await expect(page.locator('.tiles .ib').nth(1).locator('.c')).toHaveText('0');

	// an open sync-gap alert links to review
	sql(
		`insert into review_item (household_id, profile_id, kind, ref_table, ref_id, payload) select household_id, id, 'source_gap', 'source', '${source}', '{}' from profile where id = '${profile}'`
	);
	await page.reload();
	await expect(card).toContainText('1 sync gap alert is open.');
	await expect(card.getByRole('link', { name: 'Open in review →' })).toHaveAttribute('href', '/import-review');

	// the same card in Spanish
	await page.goto('/connections?lang=es');
	await expect(page.getByRole('heading', { name: 'Conexiones', level: 1 })).toBeVisible();
	await expect(card).toContainText('Hay 1 aviso de hueco de sincronización abierto.');
	await expect(card.getByRole('button', { name: 'Reconectar' })).toBeVisible();
	await expect(card).toContainText('Historia clínica (SMART on FHIR)');

	// disconnect: the vendor revokes, the tokens go, the card says so
	await page.goto('/connections?lang=en');
	await card.getByRole('button', { name: 'Disconnect' }).click();
	await card.getByRole('group').getByRole('button', { name: 'Disconnect' }).click();
	await expect(card).toContainText('Hospital LabNorte is disconnected, and the vendor revoked the access.');
	await expect(card).toHaveAttribute('data-state', 'off');
	expect(sql("select count(*) from secret where purpose = 'connector_refresh_token'")).toBe('0');
	const book = (await (await page.request.get(`${fake}/__book`)).json()) as { grants: number; revoked: number };
	expect(book).toMatchObject({ grants: 2, revoked: 1 });
	expect(errors).toEqual([]);
});

const CODES = [
	{ code: 'another_patient', en: 'That account at the vendor is for another patient.', es: 'Esa cuenta del proveedor es de otro paciente.' },
	{ code: 'denied', en: 'The vendor did not give access', es: 'El proveedor no dio acceso' },
	{ code: 'state_invalid', en: 'This sign-in expired or was already used.', es: 'Este inicio de sesión caducó o ya se usó.' },
	{ code: 'no_such_code', en: 'The connection did not finish (code no_such_code).', es: 'La conexión no terminó (código no_such_code).' }
];

for (const lang of ['en', 'es'] as const) {
	test(`the callback's error codes read in ${lang}`, async ({ page }) => {
		const errors = collectPageErrors(page);
		for (const c of CODES) {
			await page.goto(`/connections?connector_error=${c.code}&lang=${lang}`);
			const alert = page.getByRole('alert');
			await expect(alert).toContainText(c[lang]);
			await expect(page).toHaveURL(`${app}/connections`);
		}
		expect(errors).toEqual([]);
	});
}
