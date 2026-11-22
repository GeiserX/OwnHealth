// The household overview: each member's row counts flags, overdue items and stale data from that
// member's own status and Due document, and names the scopes its access leaves out.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { memberCounts, showHousehold } from '../src/lib/logic/household.ts';

test('a row counts the flags, the overdue items and the stale values of the member', () => {
	const status = { data: { flags: [{ rank: 1 }, { rank: 2 }], stale: [{ name: 'TSH' }] }, withheld: { by_scope: [] } };
	const due = { items: [{ status: 'overdue' }, { status: 'due' }, { status: 'overdue' }, { status: 'up_to_date' }, { status: 'upcoming' }], withheld_scopes: [] };
	assert.deepEqual(memberCounts(status, due), { flags: 2, overdue: 2, stale: 1, withheld: [] });
});

test('only overdue items count as overdue: due, upcoming and not assessed do not', () => {
	const due = { items: [{ status: 'due' }, { status: 'upcoming' }, { status: 'not_assessed' }, { status: 'up_to_date' }] };
	assert.equal(memberCounts({}, due).overdue, 0);
});

test('what the access leaves out is named once, as scopes, from both documents', () => {
	const status = { data: { flags: [], stale: [] }, withheld: { by_scope: ['records', 'labs'] } };
	const due = { items: [], withheld_scopes: ['read:records', 'read:labs'] };
	assert.deepEqual(memberCounts(status, due).withheld, ['read:labs', 'read:records']);
});

test('missing lists count as zero, never as an error', () => {
	assert.deepEqual(memberCounts({}, {}), { flags: 0, overdue: 0, stale: 0, withheld: [] });
});

test('the overview shows for two or more profiles only', () => {
	assert.equal(showHousehold([]), false);
	assert.equal(showHousehold([{}]), false);
	assert.equal(showHousehold([{}, {}]), true);
});
