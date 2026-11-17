// Unit tests for the Settings ClinVar card's helpers (logic/inbox.ts): what the switch shows before the
// household has its source, and what a save sends.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { clinvarConfig, clinvarOn } from '../src/lib/logic/inbox.ts';

test('before the first genome the switch shows the default: on, except in the demo', () => {
	assert.equal(clinvarOn(null, false), true);
	assert.equal(clinvarOn(null, true), false);
	assert.equal(clinvarOn({ enabled: false }, false), false);
	assert.equal(clinvarOn({ enabled: true }, true), true);
});

test('a save keeps a mirror set through the API unless a path is typed', () => {
	const mirror = { url: 'http://mirror.lan/clinvar.vcf.gz', allow_private: true };
	assert.deepEqual(clinvarConfig('', mirror), mirror);
	assert.deepEqual(clinvarConfig('  ', { url: 'https://mirror.example/c.vcf.gz' }), { url: 'https://mirror.example/c.vcf.gz' });
	assert.deepEqual(clinvarConfig(' /srv/c.vcf.gz ', mirror), { path: '/srv/c.vcf.gz' });
	assert.deepEqual(clinvarConfig('', { path: '/srv/old.vcf' }), {});
	assert.deepEqual(clinvarConfig('', undefined), {});
});
