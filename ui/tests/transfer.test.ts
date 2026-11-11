// Ownership transfer at majority: what her "give access back" row says.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { backSummary, giveBackGrant, lapsed, localDay } from '../src/lib/logic/transfer.ts';

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

test('a grant whose end date has passed cannot come back as it was', () => {
	const now = new Date('2026-11-11T10:00:00Z');
	assert.equal(lapsed(null, now), false);
	assert.equal(lapsed('2026-11-12T00:00:00Z', now), false);
	assert.equal(lapsed('2026-11-11T10:00:00Z', now), true);
	assert.equal(lapsed('2026-11-01T00:00:00Z', now), true);
});

test('giving access back restores the same grant, never a wider one', () => {
	const f = { profile: 'lucia', user_id: 'u1', role: 'guardian', categories: ['read:labs'], expires_at: '2027-06-01T00:00:00Z', ended_at: '2026-11-11T00:00:00Z' };
	assert.deepEqual(giveBackGrant(f), { user_id: 'u1', profile: 'lucia', role: 'guardian', categories: ['read:labs'], expires_at: '2027-06-01T00:00:00Z' });
	// an empty set stays empty: null would mean the role's default, which for a guardian is everything
	assert.deepEqual(giveBackGrant({ ...f, categories: [], expires_at: null }).categories, []);
});
