import { expect, test } from '@playwright/test';

const user = 'c2825c8e-9b51-4195-afe5-a28b2fc556bd';
const ow = `http://127.0.0.1:${process.env.OWNHEALTH_E2E_FAKE_PORT}/api/v1/oauth`;
const callback = `http://localhost:${process.env.OWNHEALTH_E2E_PORT}/ow/api/v1/oauth/whoop/callback`;
const withingsTerms = 'https://media.withings.com/web/legal/legal-pdf/api-terms-of-use/wo-en-api-terms-of-use-20180912.pdf';
const whoopScopes = 'offline read:cycles read:sleep read:recovery read:workout';

test('a person follows WHOOP setup and confirms consent through the OwnHealth callback', async ({ page, request }) => {
	await page.goto('/connections?lang=en');
	await page.getByRole('link', { name: 'Set up a wearable' }).click();
	await page.getByRole('link', { name: 'WHOOP', exact: true }).click();
	await expect(page.getByRole('heading', { level: 1, name: 'Connect WHOOP' })).toBeVisible();
	await expect(page.getByLabel('Redirect URI', { exact: true })).toHaveValue(callback);
	await expect(page.getByLabel('OAuth scopes', { exact: true })).toHaveValue(whoopScopes);
	await expect(page.getByRole('link', { name: 'WHOOP API terms' })).toHaveAttribute('href', 'https://developer.whoop.com/api-terms-of-use/');
	await expect(page.getByText('MCP access withheld')).toBeVisible();
	await page.goto('/connections/wearables/withings?lang=en');
	await expect(page.getByRole('link', { name: 'Withings API terms' })).toHaveAttribute('href', withingsTerms);
	await expect(page.getByText('HEAD')).toHaveCount(0);

	// This is the request the guide has the operator run inside the Open Wearables container.
	const response = await request.get(`${ow}/whoop/authorize?user_id=${user}`);
	expect(response.ok()).toBeTruthy();
	const authorization = new URL((await response.json()).authorization_url);
	expect(authorization.searchParams.get('redirect_uri')).toBe(callback);
	await page.goto(authorization.href);
	const consent = page.getByRole('link', { name: 'Allow WHOOP access' });
	const callbackUrl = await consent.evaluate((link) => (link as HTMLAnchorElement).href);
	await consent.click();
	await expect(page).toHaveURL(`${new URL(callback).origin}/ow/api/v1/oauth/success?provider=whoop&user_id=${user}`);
	expect(JSON.parse((await page.locator('body').innerText()).trim())).toMatchObject({ success: true, provider: 'whoop', user_id: user });

	const replay = await request.get(callbackUrl);
	expect(replay.url()).toContain('/ow/api/v1/oauth/error?');
	expect(await replay.json()).toMatchObject({ success: false });
});

test('the Spanish guide lists the four providers and lets you copy the redirect and scopes', async ({ context, page }) => {
	await context.grantPermissions(['clipboard-read', 'clipboard-write']);
	await page.goto('/connections/wearables?lang=es');
	await expect(page.getByRole('list', { name: 'Proveedores de dispositivos' }).getByRole('link')).toHaveText(['Oura', 'Polar', 'WHOOP', 'Withings']);
	await page.getByRole('link', { name: 'WHOOP', exact: true }).click();
	await expect(page.getByRole('heading', { level: 1, name: 'Conectar WHOOP' })).toBeVisible();
	await page.getByRole('button', { name: 'Copiar URI de redirección' }).click();
	await expect(page.getByRole('status').filter({ hasText: 'Copiado' })).toBeVisible();
	expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(callback);
	await page.getByRole('button', { name: 'Copiar permisos OAuth' }).click();
	await expect(page.getByRole('status').filter({ hasText: 'Copiado' })).toBeVisible();
	expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(whoopScopes);
	await expect(page.getByText('Acceso por MCP bloqueado')).toBeVisible();
	await page.goto('/connections/wearables/strava?lang=es');
	await expect(page.getByRole('heading', { name: 'Proveedor no disponible' })).toBeVisible();
});
