// Browser checks (`pnpm run e2e`): Playwright on Chromium, dark scheme, against the real server that
// serve.sh starts on a fresh demo database. Needs `pnpm build` (server/static), the debug server binary
// (`cargo build -p ownhealth`, or OWNHEALTH_E2E_BIN), the shared test Postgres (scripts/test-db.sh)
// and Chromium (`pnpm exec playwright install chromium`, once). The e2e script type-checks this folder
// (e2e/tsconfig.json) before it runs the tests. shl.spec.ts also opens share links in a reference
// SMART Health Links viewer, which shl-viewer.sh builds once (git and npm, a few minutes) and serves.
import { defineConfig, devices } from '@playwright/test';

const port = Number(process.env.OWNHEALTH_E2E_PORT ?? 8190);
const viewerPort = Number(process.env.OWNHEALTH_E2E_VIEWER_PORT ?? port + 1);
const baseURL = `http://127.0.0.1:${port}`;
// One database name for the server and the tests (shl.spec.ts ends a link's life in it). Set here
// once: the test workers inherit it, and their own load of this file keeps it.
process.env.OWNHEALTH_E2E_DB ??= `oh_e2e_${Math.random().toString(16).slice(2, 14)}`;
process.env.OWNHEALTH_E2E_VIEWER_PORT ??= String(viewerPort);

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
			command: 'bash serve.sh',
			url: `${baseURL}/readyz`,
			env: { OWNHEALTH_E2E_PORT: String(port), OWNHEALTH_E2E_DB: process.env.OWNHEALTH_E2E_DB },
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
