import { expect, test } from '@playwright/test';
import { openAsBlob, writeFileSync } from 'node:fs';

const studyUid = '1.2.826.0.1.3680043.10.999.44';
const budgetMs = 5_000;

function report(label: string, ms: number) {
	const line = `${label}: ${Math.round(ms)} ms`;
	console.log(line);
	if (process.env.GITHUB_STEP_SUMMARY) {
		writeFileSync(process.env.GITHUB_STEP_SUMMARY, `- ${line}\n`, { flag: 'a' });
	}
}

test.skip(!process.env.OWNHEALTH_IMAGING_CD_URL, 'Needs the stack from the Imaging CD workflow.');

test('imports the synthetic 2,000-image CT CD', async ({ request }) => {
	test.setTimeout(300_000);
	const zip = process.env.OWNHEALTH_IMAGING_CD_ZIP;
	if (!zip) throw new Error('Set OWNHEALTH_IMAGING_CD_ZIP to the generated fixture.');
	const form = new FormData();
	form.set('profile', 'alex');
	form.set('files', await openAsBlob(zip, { type: 'application/zip' }), 'labnorte-ct.zip');
	const start = performance.now();
	const upload = await request.post('/api/imaging/import', { multipart: form });
	expect(upload.ok(), `CD upload returned HTTP ${upload.status()}`).toBe(true);
	const { job_id: id } = await upload.json();
	expect(id).toBeGreaterThan(0);
	let result: unknown;
	await expect(async () => {
		const response = await request.get(`/api/jobs?ids=${id}`);
		expect(response.ok(), `Import job returned HTTP ${response.status()}`).toBe(true);
		const { jobs: [job] } = await response.json();
		expect(job?.state, job?.last_error ?? 'Import job state').toBe('done');
		result = job.result;
	}).toPass({ timeout: 270_000, intervals: [250, 500, 1_000] });
	report('CT CD import (upload through completed job)', performance.now() - start);
	expect(result).toMatchObject({ imported: 1, instances: 2_000, in_review: 0, refused: 0 });
});

test('opens the CT study under five seconds', async ({ page, request, baseURL }) => {
	test.setTimeout(60_000);
	const shots = process.env.OWNHEALTH_IMAGING_CD_SHOTS;
	const lang = process.env.OWNHEALTH_IMAGING_CD_LANG ?? 'en';
	const throttled = process.env.OWNHEALTH_IMAGING_CD_THROTTLE === '1';
	if (throttled) {
		await page.route('**/ohif/t/**', async (route) => {
			if (route.request().isNavigationRequest()) await new Promise((resolve) => setTimeout(resolve, 5_250));
			await route.continue();
		});
	}

	if (shots && !throttled) {
		await page.goto(`/imaging?lang=${lang}`);
		await expect(page.getByRole('heading', { name: lang === 'es' ? 'Imagen médica' : 'Imaging', exact: true }))
			.toBeVisible();
		await page.screenshot({ path: `${shots}/imaging-${lang}.png` });
	}

	// Include the same link-minting POST the Open button makes in the elapsed time.
	const start = performance.now();
	const open = await request.post(`/api/profiles/alex/imaging/studies/${studyUid}/open`);
	expect(open.ok(), `Viewer link returned HTTP ${open.status()}`).toBe(true);
	const { viewer_url: viewerPath } = await open.json();
	expect(typeof viewerPath === 'string' && viewerPath.startsWith('/ohif/t/')).toBe(true);
	try {
		const response = await page.goto(new URL(viewerPath, baseURL).href, { waitUntil: 'commit' });
		expect(response?.ok()).toBe(true);
	} catch {
		// Playwright navigation errors can include the viewer capability.
		throw new Error('The viewer document could not be opened.');
	}

	try {
		await page.waitForFunction(() => {
			const sample = document.createElement('canvas');
			sample.width = sample.height = 32;
			const context = sample.getContext('2d', { willReadFrequently: true });
			if (!context) return false;
			for (const canvas of document.querySelectorAll('canvas.cornerstone-canvas')) {
				if (!(canvas instanceof HTMLCanvasElement) || !canvas.checkVisibility()
					|| canvas.clientWidth < 128 || canvas.clientHeight < 128) continue;
				context.clearRect(0, 0, 32, 32);
				context.drawImage(canvas, canvas.width / 4, canvas.height / 4,
					canvas.width / 2, canvas.height / 2, 0, 0, 32, 32);
				const pixels = context.getImageData(0, 0, 32, 32).data;
				let light = 0;
				let dark = 0;
				for (let i = 0; i < pixels.length; i += 4) {
					if (pixels[i] >= 128) light++;
					if (pixels[i] <= 64) dark++;
				}
				// The synthetic CT has light rings over a darker background. A blank or flat canvas fails.
				if (light >= 64 && dark >= 64) return true;
			}
			return false;
		}, undefined, { timeout: 35_000 });
	} catch (error) {
		report('Timed out waiting for a drawn CT frame', performance.now() - start);
		if (shots) await page.screenshot({ path: `${shots}/viewer-timeout-${lang}.png` });
		throw error;
	}

	const elapsed = performance.now() - start;
	report(throttled ? 'Throttled viewer open' : 'Viewer open to first drawn CT frame', elapsed);
	if (shots && !throttled) await page.screenshot({ path: `${shots}/viewer-${lang}.png` });
	expect(elapsed, 'IMAGING_OPEN_BUDGET_EXCEEDED').toBeLessThan(budgetMs);
});
