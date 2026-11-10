// Ownership transfer at majority: who had access to a profile before its person took it over.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { formerAccess } from '../src/lib/logic/transfer.ts';

const row = (id: number, user: string, args: string, extra: Record<string, unknown> = {}) => ({
	id,
	ts: `2026-06-2${id}T10:00:00Z`,
	action: 'grant_change',
	profile: 'lucia',
	resource_ids: [user],
	args_summary: args,
	...extra
});

test('lists each guardian whose access ended at the claim, with the categories they had', () => {
	const rows = [
		row(2, 'u-alex', 'majority;role=guardian;categories=read:labs,read:summary'),
		row(1, 'u-sam', 'majority;role=guardian;categories=')
	];
	assert.deepEqual(formerAccess(rows, [], 'lucia'), [
		{ user_id: 'u-alex', role: 'guardian', categories: ['read:labs', 'read:summary'], ts: '2026-06-22T10:00:00Z' },
		{ user_id: 'u-sam', role: 'guardian', categories: [], ts: '2026-06-21T10:00:00Z' }
	]);
});

test('leaves out people who have access again, other profiles and ordinary grant changes', () => {
	const rows = [
		row(4, 'u-alex', 'majority;role=guardian;categories=read:labs'),
		row(3, 'u-sam', 'majority;role=guardian;categories=read:labs'),
		row(2, 'u-sam', 'role=viewer;categories=default'),
		row(1, 'u-ana', 'majority;role=guardian;categories=read:labs', { profile: 'mateo' }),
		row(0, 'u-eva', 'majority;role=guardian;categories=read:labs', { action: 'read' })
	];
	const grants = [{ user_id: 'u-alex', profile: 'lucia' }];
	assert.deepEqual(
		formerAccess(rows, grants, 'lucia').map((f) => f.user_id),
		['u-sam']
	);
});

test('a person listed twice shows once, from the latest row', () => {
	const rows = [row(5, 'u-sam', 'majority;role=guardian;categories=read:genome'), row(1, 'u-sam', 'majority;role=guardian;categories=read:labs')];
	assert.deepEqual(formerAccess(rows, [], 'lucia')[0].categories, ['read:genome']);
});
