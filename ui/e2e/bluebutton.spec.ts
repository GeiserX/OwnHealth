// A real OAuth callback and poll on the demo instance, with the CMS fake on loopback.
import { execFileSync } from 'node:child_process';
import { expect, test } from '@playwright/test';

const db = process.env.OWNHEALTH_E2E_DB ?? '';
const app = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_PORT ?? 8190}`;
const fake = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_FAKE_PORT ?? 9190}`;

function sql(q: string): string {
	return execFileSync('docker', ['exec', 'ownhealth-testpg', 'psql', '-v', 'ON_ERROR_STOP=1', '-U', 'postgres', '-d', db, '-qAtc', q], { encoding: 'utf8' }).trim();
}

const hex = (bytes: Uint8Array) => Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');

async function registerClient(profile: string): Promise<string> {
	// The registration wizard is a later slice. Seed a v1 household secret in the same format
	// as ownhealth_core::secrets, with a fresh value and the test instance's own key.
	const key = Uint8Array.from(atob(process.env.OWNHEALTH_E2E_SECRETS_KEY ?? ''), (c) => c.charCodeAt(0));
	const id = crypto.randomUUID();
	const nonce = crypto.getRandomValues(new Uint8Array(12));
	const purpose = 'connector_client_secret';
	const aad = new Uint8Array(16 + purpose.length);
	aad.set(Uint8Array.from(id.replaceAll('-', '').match(/../g) ?? [], (pair) => parseInt(pair, 16)));
	aad.set(new TextEncoder().encode(purpose), 16);
	const cipher = await crypto.subtle.importKey('raw', key, 'AES-GCM', false, ['encrypt']);
	const secret = new TextEncoder().encode(hex(crypto.getRandomValues(new Uint8Array(24))));
	const encrypted = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce, additionalData: aad }, cipher, secret));
	const sealed = new Uint8Array(1 + nonce.length + encrypted.length);
	sealed[0] = 1;
	sealed.set(nonce, 1);
	sealed.set(encrypted, 1 + nonce.length);
	const fingerprint = new Uint8Array(await crypto.subtle.digest('SHA-256', key)).subarray(0, 8);
	return sql(`with client_key as (
		insert into secret (id, household_id, purpose, key_fingerprint, ciphertext)
		select '${id}', household_id, '${purpose}', decode('${hex(fingerprint)}', 'hex'), decode('${hex(sealed)}', 'hex')
		from profile where id = '${profile}' returning id, household_id
	) insert into connector_client (household_id, vendor, issuer, client_id, client_secret_id, redirect_uri)
	select household_id, 'bluebutton', '${fake}/v3/fhir', 'e2e-medicare', id, '${app}/api/connectors/callback'
	from client_key returning id`);
}

let registeredClient: string | undefined;

// Other specs share this database. Remove only the rows created for this vendor client.
test.afterEach(() => {
	if (!registeredClient) return;
	sql(`begin;
		create temporary table bluebutton_e2e_source on commit drop as
			select id, secret_id from source where connector_client_id = '${registeredClient}';
		delete from review_item where payload#>>'{source,id}' in (select id::text from bluebutton_e2e_source);
		delete from document where id in (
			select document_id from document_source where source_id in (select id from bluebutton_e2e_source)
		);
		delete from source where id in (select id from bluebutton_e2e_source);
		delete from secret where id in (select secret_id from bluebutton_e2e_source);
		delete from connector_client where id = '${registeredClient}';
		commit`);
});

test('Medicare consent imports claims and shows their count in English and Spanish', async ({ page }) => {
	const profile = sql("select id from profile where slug = 'alex'");
	const client = (registeredClient = await registerClient(profile));
	await page.goto('/connections?lang=en');
	await expect(page.getByRole('heading', { name: 'Connections', level: 1 })).toBeVisible();
	const start = await page.evaluate(async ({ profile, client }) => {
		const response = await fetch('/api/connectors/launch/bluebutton', {
			method: 'POST',
			headers: { 'content-type': 'application/json', accept: 'application/json' },
			body: JSON.stringify({ profile_id: profile, client_id: client, label: 'Medicare' })
		});
		return { status: response.status, body: await response.json() as { authorize_url: string } };
	}, { profile, client });
	expect(start.status).toBe(200);
	expect(new URL(start.body.authorize_url).pathname).toBe('/v3/o/authorize/');
	await page.goto(start.body.authorize_url);
	await expect(page.getByRole('status').filter({ hasText: 'Medicare is connected.' })).toBeVisible();
	const card = page.getByRole('article', { name: 'Medicare' });
	await expect(card).toContainText('Medicare claims in last pull', { timeout: 20_000 });
	await expect(card.locator('dd').getByText('1', { exact: true })).toBeVisible();
	expect(sql("select payload->>'resource_type' || '=' || (payload->>'count') from review_item where kind = 'fhir_unmapped_summary' and payload#>>'{source,kind}' = 'bluebutton' order by 1")).toBe('Coverage=1\nExplanationOfBenefit=1');
	if (process.env.OH_BLUEBUTTON_SHOTS) await page.screenshot({ path: `${process.env.OH_BLUEBUTTON_SHOTS}/bluebutton-en.png`, fullPage: true });

	await page.goto('/connections?lang=es');
	await expect(page.getByRole('heading', { name: 'Conexiones', level: 1 })).toBeVisible();
	await expect(card).toContainText('Reclamaciones de Medicare en la última sincronización');
	const server = await card.locator('.srv').boundingBox();
	const bounds = await card.boundingBox();
	expect(server?.width ?? 0, 'the server address has room beside the Spanish label').toBeGreaterThan((bounds?.width ?? 0) / 3);
	await expect(card.locator('dd').getByText('1', { exact: true })).toBeVisible();
	if (process.env.OH_BLUEBUTTON_SHOTS) await page.screenshot({ path: `${process.env.OH_BLUEBUTTON_SHOTS}/bluebutton-es.png`, fullPage: true });
});
