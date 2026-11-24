import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
	name: 'imaging-cd',
	testDir: '.',
	testMatch: 'imaging-cd.spec.ts',
	forbidOnly: !!process.env.CI,
	workers: 1,
	reporter: 'list',
	// Failure output goes next to the regular e2e results, which git ignores.
	outputDir: 'test-results',
	use: {
		...devices['Desktop Chrome'],
		baseURL: process.env.OWNHEALTH_IMAGING_CD_URL,
		colorScheme: 'dark',
		locale: process.env.OWNHEALTH_IMAGING_CD_LANG ?? 'en',
		headless: true
	}
});
