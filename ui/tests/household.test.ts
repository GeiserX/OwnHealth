// The household overview: each member's row counts flags from that member's status, overdue items
// from the Due list and stale values from the dashboard the member's own home page shows, and names
// the scopes its access leaves out.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { memberCounts, showHousehold } from '../src/lib/logic/household.ts';

const card = (markers: { stale: boolean }[], withheld = false) => ({ withheld, markers });

test('a row counts the flags, the overdue items and the stale values of the member', () => {
	const status = { data: { flags: [{ rank: 1 }, { rank: 2 }], stale: [{ name: 'TSH' }], data_freshness: { labs: '2026-01-02' } }, withheld: { by_scope: [] } };
	const dash = {
		systems: [card([{ stale: true }, { stale: false }])],
		prevention_due: [{ status: 'overdue' }, { status: 'due' }, { status: 'overdue' }, { status: 'up_to_date' }, { status: 'upcoming' }],
		prevention_withheld: []
	};
	assert.deepEqual(memberCounts(status, dash), { flags: 2, overdue: 2, stale: 1, withheld: [], noData: false });
});

test('stale counts every stale value the dashboard marks, wearables too, as the member sees them', () => {
	// the status lists only lab recency (TSH); the dashboard also marks a sleep average stale
	const status = { data: { flags: [], stale: [{ name: 'TSH' }], data_freshness: { labs: '2026-01-02', wearables: '2026-11-01' } } };
	const dash = { systems: [card([{ stale: true }]), card([{ stale: true }, { stale: true }, { stale: false }])] };
	assert.equal(memberCounts(status, dash).stale, 3);
});

test('only overdue items count as overdue: due, upcoming and not assessed do not', () => {
	const dash = { prevention_due: [{ status: 'due' }, { status: 'upcoming' }, { status: 'not_assessed' }, { status: 'up_to_date' }] };
	assert.equal(memberCounts({}, dash).overdue, 0);
});

test('what the access leaves out is named once, as scopes, from both documents', () => {
	const status = { data: { flags: [], stale: [] }, withheld: { by_scope: ['records', 'labs'] } };
	const dash = { prevention_due: [], prevention_withheld: ['read:records', 'read:labs'] };
	assert.deepEqual(memberCounts(status, dash).withheld, ['read:labs', 'read:records']);
});

test('only a scope that feeds a column is named: wearables feed Stale, other domains feed nothing', () => {
	assert.deepEqual(memberCounts({ withheld: { by_scope: ['wearables'] } }, {}).withheld, ['read:wearables']);
	assert.deepEqual(memberCounts({ withheld: { by_scope: ['imaging', 'notes'] } }, { prevention_withheld: ['read:imaging'] }).withheld, []);
});

test('a member with no data at all says so, so the zeros do not read as up to date', () => {
	assert.equal(memberCounts({ data: { flags: [], stale: [], data_freshness: {} } }, { prevention_due: [] }).noData, true);
	// data in any domain, or a record such as a medication, is data
	assert.equal(memberCounts({ data: { data_freshness: { wearables: '2026-11-01' } } }, {}).noData, false);
	assert.equal(memberCounts({ data: { data_freshness: {}, medications: [{ name: 'Atorvastatin' }] } }, {}).noData, false);
	// a value on the dashboard is data, even if the status was read before it arrived
	assert.equal(memberCounts({ data: { data_freshness: {} } }, { systems: [card([{ stale: true }])] }).noData, false);
	assert.equal(memberCounts({ data: { data_freshness: {} } }, { systems: [card([]), card([], true)] }).noData, true);
	// what the access leaves out may hold data: that row says partial, not empty
	assert.equal(memberCounts({ data: { data_freshness: {} }, withheld: { by_scope: ['labs'] } }, {}).noData, false);
});

test('missing lists count as zero, never as an error', () => {
	assert.deepEqual(memberCounts({}, {}), { flags: 0, overdue: 0, stale: 0, withheld: [], noData: true });
});

test('the overview shows for two or more profiles only', () => {
	assert.equal(showHousehold([]), false);
	assert.equal(showHousehold([{}]), false);
	assert.equal(showHousehold([{}, {}]), true);
});
