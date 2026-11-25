// The browser checks and serve.sh share one secrets key, which the Playwright config picks. An empty
// OWNHEALTH_E2E_SECRETS_KEY counts as unset there, as it does in serve.sh; a nonempty one is kept.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const key = () => process.env.OWNHEALTH_E2E_SECRETS_KEY ?? '';

test('an empty e2e secrets key gets a fresh 32-byte key', async () => {
	process.env.OWNHEALTH_E2E_SECRETS_KEY = '';
	await import('../e2e/playwright.config.ts?empty');
	assert.equal(Buffer.from(key(), 'base64').length, 32, JSON.stringify(key()));
});

test('a set e2e secrets key is kept', async () => {
	const set = Buffer.alloc(32, 7).toString('base64');
	process.env.OWNHEALTH_E2E_SECRETS_KEY = set;
	await import('../e2e/playwright.config.ts?set');
	assert.equal(key(), set);
});
