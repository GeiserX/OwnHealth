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
	// The fake Patient matches Alex, so the real first pull records a successful sync.
	await expect.poll(() => sql(`select status from source_run where source_id = '${source}' order by id desc limit 1`), {
		timeout: 15_000
	}).toBe('ok');

	const card = page.getByRole('article', { name: 'Hospital LabNorte' });
	// a grant of 3 days shows before it ends
	await expect(card).toHaveAttribute('data-state', 'grant_soon');
	await expect(card.locator('.pill')).toHaveText('Watch');
	await expect(card).toContainText('The sign-in at the vendor ends in 3 days.');
	await expect(card).toContainText('Health record (SMART on FHIR)');
	await expect(card).toContainText(fake);
	await expect(card).toContainText('Alex Rivera');
	await expect(card).toContainText('renewed by itself');
	const reconnectButton = card.getByRole('button', { name: 'Reconnect' });
	await expect(reconnectButton).toHaveClass(/\bpri\b/); // a short grant makes Reconnect the main button

	// the vendor refused the refresh token: the screen asks for a new sign-in
	sql(`update source set reconsent_needed_at = now() where id = '${source}'`);
	await page.reload();
	await expect(card).toHaveAttribute('data-state', 'reconsent');
	await expect(card.locator('.pill')).toHaveText('Out');
	await expect(card).toContainText('The vendor no longer accepts this connection.');
	await expect(page.locator('.tiles .ib').nth(1).locator('.c')).toHaveText('1');
	await expect(reconnectButton).toHaveClass(/\bpri\b/);
	// the refresh token is refused, so the access token left is not renewed by itself
	await expect(card).toContainText('not renewed');
	await expect(card).not.toContainText('renewed by itself');

	// the reverse path: Reconnect, the vendor, the callback, and the flag is gone
	await card.getByRole('button', { name: 'Reconnect' }).click();
	await expect(page.getByRole('status').filter({ hasText: 'Hospital LabNorte is connected.' })).toBeVisible();
	await expect(card).toHaveAttribute('data-state', 'ok');
	await expect(card.locator('.pill')).toHaveText('OK');
	await expect(card).not.toContainText('no longer accepts');
	await expect(reconnectButton).not.toHaveClass(/\bpri\b/); // all is well again: Reconnect is a plain button
	expect(sql(`select reconsent_needed_at is null from source where id = '${source}'`)).toBe('t');
	await expect(page.locator('.tiles .ib').nth(1).locator('.c')).toHaveText('0');

	// an open sync-gap alert links to the review queue of Alex's profile, even with Sam's on screen
	sql(
		`insert into review_item (household_id, profile_id, kind, ref_table, ref_id, payload) select household_id, id, 'source_gap', 'source', '${source}', '{}' from profile where id = '${profile}'`
	);
	await page.evaluate(() => localStorage.setItem('oh.profile', 'sam'));
	await page.reload();
	await expect(card).toContainText('1 sync gap alert is open.');
	await card.getByRole('link', { name: 'Open in review →' }).click();
	await expect(page).toHaveURL(`${app}/import-review`);
	await expect(page.getByText('Sync gap in a connection').first()).toBeVisible();
	await page.goBack();

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
	await expect(card.locator('b[id^="con-"]')).toBeFocused(); // the button that had focus is gone
	await expect(card.getByRole('button', { name: 'Disconnect' })).toHaveCount(0);
	// switched off: the pill says Off beside the real last sync, and no grant or token dates are shown
	await expect(card.locator('.pill')).toHaveText('Off');
	await expect(card).not.toContainText('Sign-in ends');
	await expect(card).not.toContainText('Access token');
	expect(sql("select count(*) from secret where purpose = 'connector_refresh_token'")).toBe('0');
	const book = (await (await page.request.get(`${fake}/__book`)).json()) as { grants: number; revoked: number };
	expect(book).toMatchObject({ grants: 2, revoked: 1 });
	expect(errors).toEqual([]);
});

test('the clock moves while the page is open, a disconnect names where to revoke, and a refused one stays on screen', async ({ page }) => {
	const errors = collectPageErrors(page);
	expect(source, 'the first test connected a record').not.toBe('');
	await page.clock.install();
	await page.goto('/connections?lang=en');
	const card = page.getByRole('article', { name: 'Hospital LabNorte' });
	await expect(card).toHaveAttribute('data-state', 'off');
	// Reconnect switches it on again (the fake's later grants last 90 days)
	await card.getByRole('button', { name: 'Reconnect' }).click();
	await expect(card).toHaveAttribute('data-state', 'ok');

	// a grant that reaches the 7-day mark while the page is open turns to Watch without a reload, and an
	// access token past its time reads none
	sql(
		`update source set grant_expires_at = now() + interval '7 days 3 minutes', access_expires_at = now() + interval '3 minutes' where id = '${source}'`
	);
	await page.reload();
	await expect(card).toHaveAttribute('data-state', 'ok');
	await expect(card).toContainText('renewed by itself');
	await page.clock.fastForward('06:00');
	await expect(card).toHaveAttribute('data-state', 'grant_soon');
	await expect(card).toContainText('Access token none held now');

	// the vendor gave its own revocation page: the note links to it
	const revoke = 'https://vendor.example/revoke';
	let answer: { revocation: string; revoke_page: string | null } = { revocation: 'failed', revoke_page: revoke };
	await page.route(`**/api/connectors/${source}`, (route) =>
		route.request().method() === 'DELETE' ? route.fulfill({ json: { id: source, ...answer } }) : route.continue()
	);
	const disconnect = async () => {
		await card.getByRole('button', { name: 'Disconnect' }).click();
		await card.getByRole('group').getByRole('button', { name: 'Disconnect' }).click();
	};
	await disconnect();
	await expect(card.getByRole('link', { name: "The vendor's revocation page ↗" })).toHaveAttribute('href', revoke);
	// no page from the vendor: the note names the server where the access ends
	answer = { revocation: 'not_offered', revoke_page: null };
	await disconnect();
	await expect(card).toContainText(`the access ends only on the vendor's own site for ${fake}.`);

	// refused, and the card is gone after the reload: the refusal shows above the list, with focus
	await page.unroute(`**/api/connectors/${source}`);
	let gone = false;
	await page.route(`**/api/connectors/${source}`, (route) => {
		if (route.request().method() !== 'DELETE') return route.continue();
		gone = true;
		return route.fulfill({ status: 403, json: { detail: { code: 'forbidden', message: 'The role guardian cannot manage this profile.' } } });
	});
	await page.route('**/api/inboxes/health', (route) => (gone ? route.fulfill({ json: [] }) : route.continue()));
	await disconnect();
	const alert = page.getByRole('alert');
	await expect(alert).toHaveText('Hospital LabNorte: Only an owner or guardian of the profile connects its records.');
	await expect(alert).toBeFocused();
	await page.unrouteAll();

	// an address naming a connection this person does not see shows no success note
	await page.goto('/connections?connected=00000000-0000-0000-0000-000000000000&lang=en');
	await expect(card).toBeVisible();
	await expect(page).toHaveURL(`${app}/connections`);
	await expect(page.locator('.p2ok')).toHaveCount(0);
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
		// Done closes the message and leaves keyboard focus on the page heading
		await page.getByRole('alert').getByRole('button', { name: lang === 'en' ? 'Done' : 'Hecho' }).click();
		await expect(page.getByRole('alert')).toHaveCount(0);
		await expect(page.getByRole('heading', { level: 1 })).toBeFocused();
		expect(errors).toEqual([]);
	});
}

// Slice 35: one Open Wearables connection brings several vendors. When one vendor's connection ends in
// the sidecar, the others keep syncing and only the operator reconnects it there, so the card names
// the vendor, says where to fix it, and offers no Reconnect. The poller's run is seeded with psql.
for (const lang of ['en', 'es'] as const) {
	test(`an Open Wearables card names the vendor whose connection ended, in ${lang}`, async ({ page }) => {
		const errors = collectPageErrors(page);
		let ow = sql("select id from source where kind = 'openwearables'");
		if (!ow) {
			ow = sql(
				"insert into source (household_id, kind, label, profile_id, poll_seconds, reconsent_needed_at, last_run_at, last_success_at) select household_id, 'openwearables', 'Open Wearables', id, 900, now(), now(), now() from profile where slug = 'alex' returning id"
			);
			sql(
				`insert into source_run (source_id, trigger, started_at, finished_at, status, items_seen, items_new, errors) values ('${ow}', 'poll', now(), now(), 'partial', 12, 4, '[{"code": "ow_connection_ended", "message": "oura, whoop"}]')`
			);
		}
		await page.goto(`/connections?lang=${lang}`);
		const card = page.getByRole('article', { name: 'Open Wearables' });
		await expect(card).toHaveAttribute('data-state', 'reconsent');
		await expect(card).toContainText(
			lang === 'en'
				? 'Open Wearables no longer syncs Oura, WHOOP: that connection ended or was revoked there. Whoever runs this server reconnects it in Open Wearables. The other vendors keep syncing.'
				: 'Open Wearables ya no sincroniza Oura, WHOOP: esa conexión terminó o se revocó allí. Quien administra este servidor la vuelve a conectar en Open Wearables. Los demás proveedores se siguen sincronizando.'
		);
		await expect(card).not.toContainText(lang === 'en' ? 'Nothing syncs' : 'No se sincroniza nada');
		await expect(card.getByRole('button')).toHaveCount(0);
		// the later spec files share this database
		if (lang === 'es') sql(`delete from source where id = '${ow}'`);
		expect(errors).toEqual([]);
	});
}
