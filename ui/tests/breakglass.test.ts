// Break-glass on screen: where a request stands, the outbox grouped per request, who may be named.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
	NOTICES_SHOWN,
	canLogIn,
	contactChoices,
	endErrorKey,
	groupNotices,
	noticeLoader,
	ownerStep,
	phase,
	statusLine,
	unreadGroups,
	type NoticeView
} from '../src/lib/logic/breakglass.ts';
import type { Notice, NoticeUses, Notices } from '../src/lib/api/types.ts';

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
	assert.equal(g[0].unread, 21);
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

test('the read count comes from the server count over every read, not from the reads the list holds', () => {
	// 200 reads in the list, 220 in all; an older unread step of the same request is kept
	const items = [
		...Array.from({ length: 200 }, (_, i) => notice('break_glass.use', new Date(Date.parse('2026-11-24T10:00:00Z') + i * 1000).toISOString())),
		notice('break_glass.revoke', '2026-11-20T10:00:00Z', { actor: { user_id: 'u-co', display_name: 'Co Owner' } })
	];
	const uses: NoticeUses[] = [
		{ request_id: 'r1', profile: alex, contact: { user_id: 'u-pat', display_name: 'Pat Rivera' }, count: 220, unread: 3, last_at: '2026-11-24T10:03:19Z' },
		// a request with unread reads and nothing else in the list still gets its entry
		{ request_id: 'r9', profile: alex, contact: { user_id: 'u-sam', display_name: 'Sam Rivera' }, count: 4, unread: 4, last_at: '2026-11-23T08:00:00Z' }
	];
	const g = groupNotices(items, uses);
	assert.deepEqual(g.map((x) => [x.request, x.uses, x.lastUse]), [['r1', 220, '2026-11-24T10:03:19Z'], ['r9', 4, '2026-11-23T08:00:00Z']]);
	assert.deepEqual(g[0].steps.map((s) => s.kind), ['break_glass.revoke']);
	// the revoke and the 3 unread reads; the 200 reads in the list are not counted twice
	assert.equal(g[0].unread, 4);
	assert.equal(g[1].contact, 'Sam Rivera');
	assert.equal(unreadGroups(g), 2);
});

test('notices of one second sort by instant, whether or not the server wrote a fraction', () => {
	// the server leaves out the fraction of a whole second: '...:00Z' is earlier than '...:00.5Z'
	const items = [
		notice('break_glass.request', '2026-11-24T10:00:00Z', { request_id: 'ra' }),
		notice('break_glass.activate', '2026-11-24T10:00:00.5Z', { request_id: 'ra' }),
		notice('break_glass.request', '2026-11-24T10:00:00.25Z', { request_id: 'rb' })
	];
	const g = groupNotices(items);
	assert.deepEqual(g[0].steps.map((s) => s.kind), ['break_glass.request', 'break_glass.activate']);
	assert.deepEqual(g.map((x) => x.request), ['ra', 'rb']);
	assert.equal(g[0].latest, '2026-11-24T10:00:00.5Z');
	// two requests whose newest notices fall in one second: the one with the fraction is newer, though
	// its text sorts first ('.' before 'Z')
	const two = groupNotices([
		notice('break_glass.request', '2026-11-24T10:00:00Z', { request_id: 'rc' }),
		notice('break_glass.request', '2026-11-24T10:00:00.5Z', { request_id: 'rd' })
	]);
	assert.deepEqual(two.map((x) => [x.request, x.latest]), [['rd', '2026-11-24T10:00:00.5Z'], ['rc', '2026-11-24T10:00:00Z']]);
});

test('a member who cannot log in yet cannot ask, so the owner sees it before naming them', () => {
	assert.equal(canLogIn({ user_id: 'a', display_name: 'A', enrolled: false, logins: { passkeys: 0, oidc: false, password: false } }), false);
	assert.equal(canLogIn({ user_id: 'a', display_name: 'A', enrolled: true, logins: { passkeys: 0, oidc: false, password: true } }), true);
	assert.equal(canLogIn({ user_id: 'a', display_name: 'A', logins: { passkeys: 1, oidc: false, password: false } }), true);
	assert.equal(canLogIn({ user_id: 'a', display_name: 'A', logins: { passkeys: 0, oidc: true, password: false } }), true);
	assert.equal(canLogIn({ user_id: 'a', display_name: 'A', enrolled: false }), false);
	assert.equal(canLogIn({ user_id: 'a', display_name: 'A' }), true);
});

test('a cancel the server refused because the request moved on says so in the reader language', () => {
	assert.equal(endErrorKey({ status: 409, code: 'conflict' }), 'bgl_changed');
	assert.equal(endErrorKey({ status: 409, code: 'no_calendar_feed' }), null);
	assert.equal(endErrorKey({ status: 500, code: 'internal' }), null);
	assert.equal(endErrorKey(new Error('offline')), null);
	assert.equal(endErrorKey(null), null);
});

/** A load whose answer the test hands over when it chooses. */
function held() {
	const calls: { resolve: (n: Notices) => void; reject: (e: Error) => void }[] = [];
	const fetch = () => new Promise<Notices>((resolve, reject) => calls.push({ resolve, reject }));
	return { calls, fetch };
}
const unreadList = (id: string): Notices => ({ items: [notice('break_glass.request', '2026-11-24T10:00:00Z', { id })], uses: [], unread: 1 });
const readList = (id: string): Notices => ({ items: [notice('break_glass.request', '2026-11-24T10:00:00Z', { id, read_at: '2026-11-24T11:00:00Z' })], uses: [], unread: 0 });
const memory = () => {
	const m = new Map<string, string>();
	return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v), m };
};

test('a notices answer counts only for the newest load and the person still signed in', async () => {
	const v: NoticeView = { data: null, error: null, open: false };
	const h = held();
	let user = 'u-a';
	const store = memory();
	const load = noticeLoader(v, () => user, h.fetch, store);
	// a load that started before Mark all as read ends after the load that followed it: it is dropped
	const first = load();
	const second = load();
	h.calls[1].resolve(readList('after'));
	await second;
	h.calls[0].resolve(unreadList('before'));
	await first;
	assert.equal(v.data?.items[0].id, 'after');
	assert.equal(v.open, false);
	// an error of an older load does not replace the newest answer either
	const old = load();
	const latest = load();
	h.calls[3].resolve(readList('latest'));
	await latest;
	h.calls[2].reject(new Error('late'));
	await old;
	assert.equal(v.error, null);
	assert.equal(v.data?.items[0].id, 'latest');
	// the person changed while A's notices were on their way: A's never show for B, nor after a logout
	const forA = load();
	user = 'u-b';
	h.calls[4].resolve(unreadList('of-a'));
	await forA;
	assert.equal(v.data?.items[0].id, 'latest');
	assert.equal(store.getItem(NOTICES_SHOWN), null);
	const again = load();
	user = '';
	h.calls[5].resolve(unreadList('of-b'));
	await again;
	assert.equal(v.data?.items[0].id, 'latest');
	assert.equal(v.open, false);
	// nobody signed in: nothing is read
	await load();
	assert.equal(h.calls.length, 6);
});

test('unread notices open the panel after the first load that works, once per login', async () => {
	const v: NoticeView = { data: null, error: null, open: false };
	const h = held();
	const store = memory();
	const load = noticeLoader(v, () => 'u-a', h.fetch, store);
	// the first load fails: nothing opens, the error shows
	const failed = load();
	h.calls[0].reject(new Error('offline'));
	await failed;
	assert.equal(v.open, false);
	assert.ok(v.error);
	// a later refresh brings unread notices: they open now, and the mark is kept
	const later = load();
	h.calls[1].resolve(unreadList('n'));
	await later;
	assert.equal(v.open, true);
	assert.equal(v.error, null);
	assert.equal(store.getItem(NOTICES_SHOWN), 'u-a');
	// closed by the person: the next refresh of the same login leaves it closed
	v.open = false;
	const next = load();
	h.calls[2].resolve(unreadList('n'));
	await next;
	assert.equal(v.open, false);
	// another login in this browser opens its own once
	const other = noticeLoader(v, () => 'u-b', h.fetch, store);
	const forB = other();
	h.calls[3].resolve(unreadList('m'));
	await forB;
	assert.equal(v.open, true);
	assert.equal(store.getItem(NOTICES_SHOWN), 'u-b');
});
