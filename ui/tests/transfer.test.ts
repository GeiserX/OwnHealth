// Ownership transfer at majority: what her "give access back" row says.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { backSummary, localDay } from '../src/lib/logic/transfer.ts';

test('names every category that comes back, and flags sensitive facts and the genome', () => {
	assert.deepEqual(backSummary(['read:labs', 'read:summary']), { keys: ['fu_sc_read_labs', 'fu_sc_read_summary'], sensitive: false });
	assert.equal(backSummary(['read:labs', 'read:sensitive']).sensitive, true);
	assert.equal(backSummary(['read:genome_raw']).sensitive, true);
	assert.deepEqual(backSummary([]), { keys: [], sensitive: false });
});

test('the end date is the local day, not the UTC one', () => {
	const prev = process.env.TZ;
	try {
		// 00:10 on 11 November in Madrid is still 10 November in UTC
		process.env.TZ = 'Europe/Madrid';
		assert.equal(localDay('2026-11-10T23:10:00Z'), '2026-11-11');
		process.env.TZ = 'UTC';
		assert.equal(localDay('2026-11-10T23:10:00Z'), '2026-11-10');
		assert.equal(localDay('not a date'), '—');
	} finally {
		if (prev === undefined) delete process.env.TZ;
		else process.env.TZ = prev;
	}
});
