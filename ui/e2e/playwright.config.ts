// Browser checks (`pnpm run e2e`): Playwright on Chromium, dark scheme, against the real server that
// serve.sh starts on a fresh demo database. Needs `pnpm build` (server/static), the debug server binary
// (`cargo build -p ownhealth`, or OWNHEALTH_E2E_BIN), the shared test Postgres (scripts/test-db.sh)
// and Chromium (`pnpm exec playwright install chromium`, once). The e2e script type-checks this folder
// (e2e/tsconfig.json) before it runs the tests. The connections checks also start fake-vendor.mjs, a
// fake SMART vendor on loopback, and seed rows with psql into the database serve.sh creates: its name
// is picked here, so the tests in the workers know it too.
import { defineConfig, devices } from '@playwright/test';

const port = Number(process.env.OWNHEALTH_E2E_PORT ?? 8190);
const baseURL = `http://127.0.0.1:${port}`;
const fakePort = Number(process.env.OWNHEALTH_E2E_FAKE_PORT ?? port + 1000);
process.env.OWNHEALTH_E2E_PORT = String(port);
process.env.OWNHEALTH_E2E_FAKE_PORT = String(fakePort);
// The workers load this file again and inherit the runner's environment, so they keep this name.
process.env.OWNHEALTH_E2E_DB ??= `oh_e2e_${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;

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
			command: 'bash serve.sh',
			url: `${baseURL}/readyz`,
			env: { OWNHEALTH_E2E_PORT: String(port), OWNHEALTH_E2E_DB: process.env.OWNHEALTH_E2E_DB },
			reuseExistingServer: false,
			timeout: 120_000,
			gracefulShutdown: { signal: 'SIGTERM', timeout: 10_000 },
			stdout: 'ignore',
			stderr: 'pipe'
		}
	]
});
