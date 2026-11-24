// Browser checks (`pnpm run e2e`): Playwright on Chromium, dark scheme, against the real server that
// serve.sh starts on a fresh demo database. Needs `pnpm build` (server/static), the debug server binary
// (`cargo build -p ownhealth`, or OWNHEALTH_E2E_BIN), the shared test Postgres (scripts/test-db.sh)
// and Chromium (`pnpm exec playwright install chromium`, once). The e2e script type-checks this folder
// (e2e/tsconfig.json) before it runs the tests. shl.spec.ts also opens share links in a reference
// SMART Health Links viewer, which shl-viewer.sh builds once (git and npm, a few minutes) and serves.
// The connections checks also start fake-vendor.mjs, a fake SMART vendor on loopback, and seed rows
// with psql into the database serve.sh creates: its name is picked here, so the workers know it too.
// The imaging checks open a study through fake-orthanc.mjs, a fake Orthanc that serve.sh points the
// server at (imaging on), with a password picked here for both.
import { defineConfig, devices } from '@playwright/test';
import { randomBytes } from 'node:crypto';
import { tmpdir } from 'node:os';

const port = Number(process.env.OWNHEALTH_E2E_PORT ?? 8190);
const viewerPort = Number(process.env.OWNHEALTH_E2E_VIEWER_PORT ?? port + 1);
const baseURL = `http://127.0.0.1:${port}`;
const fakePort = Number(process.env.OWNHEALTH_E2E_FAKE_PORT ?? port + 1000);
const orthancPort = Number(process.env.OWNHEALTH_E2E_ORTHANC_PORT ?? port + 2000);
process.env.OWNHEALTH_E2E_PORT = String(port);
process.env.OWNHEALTH_E2E_FAKE_PORT = String(fakePort);
process.env.OWNHEALTH_E2E_ORTHANC_PORT = String(orthancPort);
// the workers load this file again and inherit the runner's environment, so they keep this password
process.env.OWNHEALTH_E2E_ORTHANC_PASSWORD ??= randomBytes(16).toString('hex');
process.env.OWNHEALTH_E2E_VIEWER_PORT ??= String(viewerPort);
// One database name for the server and the tests (shl.spec.ts ends a link's life in it, the
// connections checks seed rows with psql). The workers load this file again and inherit the
// runner's environment, so they keep this name.
process.env.OWNHEALTH_E2E_DB ??= `oh_e2e_${Math.random().toString(16).slice(2, 14)}`;
// Where the server looks for the audit anchor a backup writes (OWNHEALTH_ANCHOR_DIR): the audit
// checks write one there by hand. serve.sh removes it on exit when it was not there before.
process.env.OWNHEALTH_E2E_ANCHOR_DIR ??= `${tmpdir()}/${process.env.OWNHEALTH_E2E_DB}-anchor`;

export default defineConfig({
	testDir: '.',
	forbidOnly: !!process.env.CI,
	workers: 1,
	reporter: 'list',
	outputDir: 'test-results',
	use: { baseURL, colorScheme: 'dark', trace: 'retain-on-failure' },
	projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'], colorScheme: 'dark' } }],
	webServer: [
		{
			command: 'node fake-vendor.mjs',
			url: `http://127.0.0.1:${fakePort}/.well-known/smart-configuration`,
			env: { OWNHEALTH_E2E_FAKE_PORT: String(fakePort) },
			reuseExistingServer: false,
			timeout: 30_000,
			stdout: 'ignore',
			stderr: 'pipe'
		},
		{
			command: 'node fake-orthanc.mjs',
			url: `http://127.0.0.1:${orthancPort}/healthz`,
			env: { OWNHEALTH_E2E_ORTHANC_PORT: String(orthancPort), OWNHEALTH_E2E_ORTHANC_PASSWORD: process.env.OWNHEALTH_E2E_ORTHANC_PASSWORD },
			reuseExistingServer: false,
			timeout: 30_000,
			stdout: 'ignore',
			stderr: 'pipe'
		},
		{
			command: 'bash serve.sh',
			url: `${baseURL}/readyz`,
			env: {
				OWNHEALTH_E2E_PORT: String(port),
				OWNHEALTH_E2E_DB: process.env.OWNHEALTH_E2E_DB,
				OWNHEALTH_E2E_ANCHOR_DIR: process.env.OWNHEALTH_E2E_ANCHOR_DIR,
				OWNHEALTH_E2E_ORTHANC_PORT: String(orthancPort),
				OWNHEALTH_E2E_ORTHANC_PASSWORD: process.env.OWNHEALTH_E2E_ORTHANC_PASSWORD
			},
			reuseExistingServer: false,
			timeout: 120_000,
			gracefulShutdown: { signal: 'SIGTERM', timeout: 10_000 },
			stdout: 'ignore',
			stderr: 'pipe'
		},
		{
			command: `bash shl-viewer.sh serve ${viewerPort}`,
			url: `http://127.0.0.1:${viewerPort}/index.html`,
			reuseExistingServer: false,
			// the first run builds the viewer
			timeout: 900_000,
			gracefulShutdown: { signal: 'SIGTERM', timeout: 5_000 },
			stdout: 'ignore',
			stderr: 'pipe'
		}
	]
});
