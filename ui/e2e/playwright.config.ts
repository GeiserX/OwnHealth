// Browser checks (`pnpm run e2e`): Playwright on Chromium, dark scheme, against the real server that
// serve.sh starts on a fresh demo database. Needs `pnpm build` (server/static), the debug server binary
// (`cargo build -p ownhealth`, or OWNHEALTH_E2E_BIN), the shared test Postgres (scripts/test-db.sh)
// and Chromium (`pnpm exec playwright install chromium`, once). The e2e script type-checks this folder
// (e2e/tsconfig.json) before it runs the tests.
import { defineConfig, devices } from '@playwright/test';

const port = Number(process.env.OWNHEALTH_E2E_PORT ?? 8190);
const baseURL = `http://127.0.0.1:${port}`;

export default defineConfig({
	testDir: '.',
	forbidOnly: !!process.env.CI,
	workers: 1,
	reporter: 'list',
	outputDir: 'test-results',
	use: { baseURL, colorScheme: 'dark', trace: 'retain-on-failure' },
	projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'], colorScheme: 'dark' } }],
	webServer: {
		command: 'bash serve.sh',
		url: `${baseURL}/readyz`,
		env: { OWNHEALTH_E2E_PORT: String(port) },
		reuseExistingServer: false,
		timeout: 120_000,
		gracefulShutdown: { signal: 'SIGTERM', timeout: 10_000 },
		stdout: 'ignore',
		stderr: 'pipe'
	}
});
