// The health card panel with a SMART Health Link (slice 23), with POST /api/shc answered in the page so
// each answer is the one the step needs: a passcode typed for one link is never kept for the next
// one, and the issuer questions of a link whose files are held stay when the approval request fails.
// The rest of the app is the real server on the demo household; no request leaves this machine.
import { expect, test, type Page, type Request, type Route } from '@playwright/test';

const HELD = [{ sha: 'a'.repeat(64), content_type: 'application/smart-health-card' }];
const link = (name: string) => ({ host: `shl-${name}.labnorte.example`, label: `Link ${name}`, expires: null, passcode: true, direct: false });
const asksLink = (name: string) => ({ status: 'needs_approval', hosts: [{ host: link(name).host, issuers: [] }], link: link(name) });
const asksIssuer = { status: 'needs_approval', hosts: [{ host: 'issuer.labnorte.example', issuers: ['https://issuer.labnorte.example'] }], link: link('c'), held: HELD };

/** The fields of a multipart form body, by name, in order. */
function fields(req: Request): Record<string, string[]> {
	const out: Record<string, string[]> = {};
	for (const m of (req.postData() ?? '').matchAll(/name="([^"]+)"\r\n\r\n([^\r]*)\r\n/g)) (out[m[1]] ??= []).push(m[2]);
	return out;
}

/** Each POST /api/shc gets the next answer: a body, or an error status with its body. Every form is kept. */
type Answer = object | { status: number; detail: object };
async function answer(page: Page, answers: Answer[]): Promise<Record<string, string[]>[]> {
	const sent: Record<string, string[]>[] = [];
	await page.route('**/api/shc', async (route: Route) => {
		sent.push(fields(route.request()));
		const next = answers.shift() ?? {};
		if ('detail' in next && 'status' in next && typeof next.status === 'number') await route.fulfill({ status: next.status, json: { detail: next.detail } });
		else await route.fulfill({ json: next });
	});
	return sent;
}

const failed = { status: 500, detail: { code: 'internal', message: 'Something went wrong.' } };
const inactive = { status: 422, detail: { code: 'shl_refused', reason: 'inactive', detail: null, message: 'the link is no longer active' } };

async function openPanel(page: Page) {
	await page.goto('/import-review?lang=en');
	await page.getByRole('button', { name: 'Alex Rivera', exact: true }).click();
	await page.getByRole('button', { name: 'Health card', exact: true }).click();
}

async function paste(page: Page, text: string) {
	await page.locator('#shc-text').fill(text);
	await page.getByRole('button', { name: 'Open pasted link', exact: true }).click();
}

test("a passcode typed for one link is not kept for the next link, after a decline or a new read", async ({ page }) => {
	const sent = await answer(page, [asksLink('a'), asksLink('b'), asksLink('c'), inactive]);
	await openPanel(page);
	await paste(page, 'shlink:/link-a');
	await expect(page.getByText('Open this SMART Health Link?')).toBeVisible();
	await page.locator('#shl-passcode').fill('test-pin-aaaa');
	await page.getByRole('button', { name: 'Do not fetch', exact: true }).click();
	// link B after the decline: its field is empty and its approve button waits for a passcode
	await paste(page, 'shlink:/link-b');
	await expect(page.getByRole('button', { name: `Open the link at ${link('b').host}` })).toBeVisible();
	await expect(page.locator('#shl-passcode')).toHaveValue('');
	await expect(page.getByRole('button', { name: `Open the link at ${link('b').host}` })).toBeDisabled();
	// link C read straight over B's question, with a passcode typed for B
	await page.locator('#shl-passcode').fill('test-pin-bbbb');
	await paste(page, 'shlink:/link-c');
	await expect(page.getByRole('button', { name: `Open the link at ${link('c').host}` })).toBeVisible();
	await expect(page.locator('#shl-passcode')).toHaveValue('');
	await page.locator('#shl-passcode').fill('test-pin-cccc');
	await page.getByRole('button', { name: `Open the link at ${link('c').host}` }).click();
	await expect.poll(() => sent.length).toBe(4);
	// each read went without a passcode; the one approval carried C's own
	expect(sent.slice(0, 3).map((f) => f.passcode)).toEqual([undefined, undefined, undefined]);
	expect(sent[3].passcode).toEqual(['test-pin-cccc']);
	expect(JSON.stringify(sent)).not.toContain('test-pin-aaaa');
	expect(JSON.stringify(sent)).not.toContain('test-pin-bbbb');
});

test('the issuer questions of a link with held files stay when the approval request fails', async ({ page }) => {
	const sent = await answer(page, [asksIssuer, failed, asksIssuer]);
	await openPanel(page);
	await paste(page, 'shlink:/link-c');
	const fetchKeys = page.getByRole('button', { name: 'Fetch keys from issuer.labnorte.example', exact: true });
	await expect(fetchKeys).toBeVisible();
	await fetchKeys.click();
	await expect(page.locator('section.shc').getByRole('alert')).toBeVisible();
	// the question is still there, and asking again sends the held files again
	await expect(fetchKeys).toBeVisible();
	await fetchKeys.click();
	await expect.poll(() => sent.length).toBe(3);
	for (const f of sent.slice(1)) expect(f.held).toEqual([`${HELD[0].sha} ${HELD[0].content_type}`]);
	expect(sent[2].approve).toEqual(['issuer.labnorte.example']);
});
