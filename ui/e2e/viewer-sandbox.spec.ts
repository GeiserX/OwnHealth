// The imaging viewer runs sandboxed (server/crates/ownhealth-api/src/routes/imaging_proxy.rs): every
// answer under /ohif carries `Content-Security-Policy: sandbox allow-scripts`, refusals included, so the
// page runs in an opaque origin and cannot use the person's credentials. Neither path below is a live
// link, which is enough: /ohif answers a refusal page with the same header. imaging.spec.ts opens a
// live one.
import { expect, test, type Page } from '@playwright/test';

/** What a page can do with the person's credentials: its origin, the cookie jar, and `/api/me`. */
function probe(page: Page) {
	return page.evaluate(async () => {
		let cookie: string;
		try {
			cookie = `readable (${document.cookie.length} chars)`;
		} catch {
			cookie = 'blocked';
		}
		let api: string;
		try {
			const r = await fetch('/api/me', { credentials: 'include' });
			api = `read ${r.status}`;
		} catch {
			api = 'blocked';
		}
		return { origin: window.origin, cookie, api };
	});
}

test('a page under /ohif runs in an opaque origin and cannot read the API with credentials', async ({
	page
}) => {
	// The control: the app's own page reads /api/me with its credentials, so the probe can see a read.
	await page.goto('/?lang=en');
	await expect(page.getByRole('button', { name: 'Alex Rivera', exact: true })).toBeVisible();
	const own = await probe(page);
	expect(own.origin).toBe(new URL(page.url()).origin);
	expect(own.api).toBe('read 200');

	for (const path of ['/ohif/', '/ohif/t/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/viewer']) {
		const res = await page.goto(path);
		expect(res?.headers()['content-security-policy'] ?? '', path).toMatch(/^sandbox allow-scripts;/);
		expect(await probe(page), path).toEqual({ origin: 'null', cookie: 'blocked', api: 'blocked' });
	}
});
