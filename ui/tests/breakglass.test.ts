// Break-glass on screen: where a request stands, the outbox grouped per request, who may be named.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { contactChoices, findRequest, groupNotices, ownerStep, phase, statusLine, unreadGroups } from '../src/lib/logic/breakglass.ts';
import type { Notice } from '../src/lib/api/types.ts';

const now = new Date('2026-11-24T12:00:00Z');
const before = '2026-11-24T11:00:00Z';
const after = '2026-11-24T13:00:00Z';
const req = (over: Partial<Parameters<typeof phase>[0]> = {}) => ({
	status: 'requested' as const,
	activates_at: after,
	owner_feed_at: null,
	waiting_for_calendar: false,
	expires_at: null,
	ended_at: null,
	...over
});

test('a request waits for its delay, then for the owner calendar, then opens', () => {
	assert.equal(phase(req(), now), 'waiting');
	// the delay has passed, but no owner calendar feed served it: the owner decision on PR 50
	assert.equal(phase(req({ activates_at: before }), now), 'calendar');
	assert.equal(phase(req({ waiting_for_calendar: true }), now), 'calendar');
	// both hold: the server opens it at its next request
	assert.equal(phase(req({ activates_at: before, owner_feed_at: before }), now), 'opening');
	// a feed that served it early does not open it before its delay
	assert.equal(phase(req({ owner_feed_at: before }), now), 'waiting');
	assert.equal(phase(req({ status: 'active', expires_at: after }), now), 'active');
	assert.equal(phase(req({ status: 'active', expires_at: before }), now), 'expired');
	for (const s of ['cancelled', 'expired', 'revoked'] as const) assert.equal(phase(req({ status: s }), now), s);
});

test('the status line names the calendar it waits for, and the time that matters', () => {
	const cal = req({ activates_at: before });
	assert.deepEqual(statusLine(cal, 'owner', now), { key: 'bgl_st_calendar_owner', at: before });
	assert.deepEqual(statusLine(cal, 'contact', now), { key: 'bgl_st_calendar_contact', at: before });
	assert.deepEqual(statusLine(req(), 'owner', now), { key: 'bgl_st_waiting', at: after });
	assert.deepEqual(statusLine(req({ status: 'active', expires_at: after }), 'contact', now), { key: 'bgl_st_active', at: after });
	assert.deepEqual(statusLine(req({ status: 'cancelled', ended_at: before }), 'owner', now), { key: 'bgl_st_cancelled', at: before });
	assert.deepEqual(statusLine(req({ status: 'active', expires_at: before }), 'owner', now), { key: 'bgl_st_expired', at: before });
});

test('the owner cancels a waiting request and revokes an open one; a closed one has no step', () => {
	assert.equal(ownerStep(req(), now), 'cancel');
	assert.equal(ownerStep(req({ activates_at: before }), now), 'cancel');
	assert.equal(ownerStep(req({ status: 'active', expires_at: after }), now), 'revoke');
	assert.equal(ownerStep(req({ status: 'active', expires_at: before }), now), null);
	assert.equal(ownerStep(req({ status: 'revoked' }), now), null);
});

const alex = { id: 'p1', slug: 'alex', display_name: 'Alex Rivera' };
let seq = 0;
const notice = (kind: Notice['kind'], at: string, over: Partial<Notice> = {}): Notice => ({
	id: `n${++seq}`,
	kind,
	profile: alex,
	request_id: 'r1',
	actor: null,
	contact: { user_id: 'u-pat', display_name: 'Pat Rivera' },
	created_at: at,
	read_at: null,
	...over
});

test('reads through one request show as one line with their count, and the badge counts requests', () => {
	const items = [
		...Array.from({ length: 20 }, (_, i) => notice('break_glass.use', `2026-11-24T10:${String(10 + i).padStart(2, '0')}:00Z`)),
		notice('break_glass.activate', '2026-11-24T10:05:00Z'),
		notice('break_glass.request', '2026-11-22T10:00:00Z', { actor: { user_id: 'u-pat', display_name: 'Pat Rivera' }, read_at: '2026-11-22T11:00:00Z' }),
		notice('break_glass.request', '2026-11-23T09:00:00Z', { request_id: 'r2', contact: { user_id: 'u-sam', display_name: 'Sam Rivera' } })
	];
	const g = groupNotices(items);
	assert.equal(g.length, 2);
	assert.equal(unreadGroups(g), 2);
	// newest first: r1's last read is newer than r2's request
	assert.deepEqual(g.map((x) => x.request), ['r1', 'r2']);
	assert.equal(g[0].uses, 20);
	assert.equal(g[0].lastUse, '2026-11-24T10:29:00Z');
	assert.equal(g[0].contact, 'Pat Rivera');
	assert.equal(g[1].contact, 'Sam Rivera');
	// the steps read in order, and the ones read already are marked so
	assert.deepEqual(g[0].steps.map((s) => [s.kind, s.unread]), [['break_glass.request', false], ['break_glass.activate', true]]);
	assert.equal(g[0].unread.length, 21);
	const read = groupNotices(items.map((n) => (n.request_id === 'r2' ? n : { ...n, read_at: '2026-11-24T11:00:00Z' })));
	assert.equal(unreadGroups(read), 1);
	assert.equal(unreadGroups([]), 0);
});

test('the owner may name any other login of the household that does not own the profile yet', () => {
	const members = [
		{ user_id: 'u-alex', display_name: 'Alex Rivera', own_profile: 'alex' },
		{ user_id: 'u-sam', display_name: 'Sam Rivera', own_profile: 'sam' },
		{ user_id: 'u-pat', display_name: 'Pat Rivera' },
		{ user_id: 'u-co', display_name: 'Co Owner' },
		{ user_id: 'u-named', display_name: 'Named Already' }
	];
	const grants = [
		{ user_id: 'u-co', profile: 'alex', role: 'owner' },
		{ user_id: 'u-sam', profile: 'alex', role: 'viewer' }
	];
	const ids = contactChoices(members, grants, 'alex', 'u-alex', ['u-named']).map((m) => m.user_id);
	assert.deepEqual(ids, ['u-sam', 'u-pat']);
});

test('the cancel link finds its request on whichever owned profile holds it', () => {
	const views = [
		{ slug: 'alex', open_requests: [{ id: 'a' }], requests: [{ id: 'b' }] },
		{ slug: 'lucia', open_requests: [], requests: [{ id: 'c' }] }
	];
	assert.deepEqual(findRequest(views, 'c'), { slug: 'lucia', request: { id: 'c' } });
	assert.deepEqual(findRequest(views, 'b'), { slug: 'alex', request: { id: 'b' } });
	assert.equal(findRequest(views, 'zz'), null);
});
