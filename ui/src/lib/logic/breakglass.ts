// Break-glass on screen (routes/break_glass.rs): where a request stands, the notices grouped per
// request, and who the owner may name. Pure, so the tests can run it without a browser.
import type { BreakGlassRequest, Notice, NoticeIce, NoticeKind, NoticeUses, Notices } from '../api/types';

type Req = Pick<BreakGlassRequest, 'status' | 'activates_at' | 'owner_feed_at' | 'waiting_for_calendar' | 'expires_at' | 'ended_at'>;

/**
 * Where a request stands now. `waiting`: the delay runs. `calendar`: the delay has passed, but no
 * owner calendar feed has served the request yet, so it cannot open (the owner's decision on PR 50).
 * `opening`: both hold; the server opens it at its next request or its minute sweep. A page left open
 * moves on with the clock, so the server's own `waiting_for_calendar` is not the only signal.
 */
export type Phase = 'waiting' | 'calendar' | 'opening' | 'active' | 'cancelled' | 'expired' | 'revoked';

export function phase(r: Req, now: Date = new Date()): Phase {
	if (r.status === 'requested') {
		const due = new Date(r.activates_at).getTime() <= now.getTime();
		if (r.waiting_for_calendar || (due && !r.owner_feed_at)) return 'calendar';
		return due ? 'opening' : 'waiting';
	}
	// an open request whose 14 days ran out, before the sweep wrote it
	if (r.status === 'active' && r.expires_at && new Date(r.expires_at).getTime() <= now.getTime()) return 'expired';
	return r.status;
}

/** Whether the owner can still stop it: cancel while it waits, revoke while it is open. */
export function ownerStep(r: Req, now: Date = new Date()): 'cancel' | 'revoke' | null {
	const ph = phase(r, now);
	if (ph === 'waiting' || ph === 'calendar' || ph === 'opening') return 'cancel';
	return ph === 'active' ? 'revoke' : null;
}

/**
 * The dict key of the status line, and the instant it names: when access opens (waiting), until when
 * it is open (active), when it ended (cancelled, expired, revoked). `who` picks the calendar line:
 * the owner's own calendar app, or the owner's for the contact.
 */
export function statusLine(r: Req, who: 'owner' | 'contact', now: Date = new Date()): { key: string; at: string | null } {
	const ph = phase(r, now);
	switch (ph) {
		case 'waiting':
			return { key: 'bgl_st_waiting', at: r.activates_at };
		case 'calendar':
			return { key: `bgl_st_calendar_${who}`, at: r.activates_at };
		case 'opening':
			return { key: 'bgl_st_opening', at: r.activates_at };
		case 'active':
			return { key: 'bgl_st_active', at: r.expires_at };
		case 'expired':
			return { key: 'bgl_st_expired', at: r.ended_at ?? r.expires_at };
		default:
			return { key: `bgl_st_${ph}`, at: r.ended_at };
	}
}

/**
 * The dict key for a cancel or an end the server refused because the request moved on while the page
 * was open (409 `conflict`: it opened, ran out or another owner ended it); null for any other error.
 */
export function endErrorKey(e: unknown): string | null {
	const x = e as { status?: unknown; code?: unknown } | null;
	return x && x.status === 409 && x.code === 'conflict' ? 'bgl_changed' : null;
}

/** A date and time in the reader's language and zone: '26 nov 2026, 10:00'. */
export function when(iso: string | null | undefined, locale: string): string {
	if (!iso) return '—';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return '—';
	return new Intl.DateTimeFormat(locale, { dateStyle: 'medium', timeStyle: 'short' }).format(d);
}

/**
 * The browser's mark that the notices opened by themselves for this login (the user id). The app
 * removes it when it finds no session, so the next login opens them again.
 */
export const NOTICES_SHOWN = 'oh.notices.shown';

/** The notices of one request, as one entry of the outbox. */
export interface NoticeGroup {
	/** the request id (a notice without one stands alone, under its own id) */
	key: string;
	request: string | null;
	/** the opens of a profile's emergency card (key `ice:<profile id>`): `uses` counts them */
	card: boolean;
	profile: Notice['profile'];
	/** who asked */
	contact: string | null;
	/** the steps other than reads, oldest first */
	steps: { kind: NoticeKind; at: string; by: string | null; unread: boolean }[];
	/** how many reads went through the request, and the last one */
	uses: number;
	lastUse: string | null;
	/** the newest notice of the group */
	latest: string;
	/** how many of its notices are unread */
	unread: number;
}

/**
 * Instants compared as instants: the server leaves the fraction out of a whole second, so the strings
 * of one second do not sort ('...:00Z' after '...:00.5Z').
 */
const ms = (iso: string): number => {
	const t = Date.parse(iso);
	return Number.isNaN(t) ? 0 : t;
};

/**
 * The outbox, one entry per request, newest first. Every read through an open request writes a
 * notice, so two page views can write twenty; they show as one line with the count. The list holds
 * only the newest notices, so the count comes from `uses`, which the server counts over every read;
 * the reads in the list count only for a request it does not cover.
 */
export function groupNotices(
	items: readonly Notice[],
	uses: readonly NoticeUses[] = [],
	ice: readonly NoticeIce[] = []
): NoticeGroup[] {
	const groups = new Map<string, NoticeGroup>();
	const counted = new Set(uses.map((u) => u.request_id));
	const cards = new Set(ice.map((i) => i.profile.id));
	const group = (key: string, request: string | null, profile: Notice['profile'], latest: string): NoticeGroup => {
		let g = groups.get(key);
		if (!g) {
			const card = key.startsWith('ice:');
			g = { key, request, card, profile, contact: null, steps: [], uses: 0, lastUse: null, latest, unread: 0 };
			groups.set(key, g);
		}
		return g;
	};
	for (const n of items) {
		// the opens of an emergency card have no request: one entry per profile, counted like reads
		if (n.kind === 'ice.access') {
			const g = group(`ice:${n.profile.id}`, null, n.profile, n.created_at);
			if (ms(n.created_at) > ms(g.latest)) g.latest = n.created_at;
			if (cards.has(n.profile.id)) continue;
			g.uses += 1;
			if (!n.read_at) g.unread += 1;
			if (!g.lastUse || ms(n.created_at) > ms(g.lastUse)) g.lastUse = n.created_at;
			continue;
		}
		const g = group(n.request_id ?? n.id, n.request_id, n.profile, n.created_at);
		g.contact ??= n.contact?.display_name ?? null;
		if (ms(n.created_at) > ms(g.latest)) g.latest = n.created_at;
		if (n.kind === 'break_glass.use') {
			if (n.request_id && counted.has(n.request_id)) continue;
			g.uses += 1;
			if (!n.read_at) g.unread += 1;
			if (!g.lastUse || ms(n.created_at) > ms(g.lastUse)) g.lastUse = n.created_at;
		} else {
			if (!n.read_at) g.unread += 1;
			g.steps.push({ kind: n.kind, at: n.created_at, by: n.actor?.display_name ?? null, unread: !n.read_at });
		}
	}
	for (const u of uses) {
		const g = group(u.request_id, u.request_id, u.profile, u.last_at);
		g.contact ??= u.contact.display_name;
		g.uses = u.count;
		g.lastUse = u.last_at;
		g.unread += u.unread;
		if (ms(u.last_at) > ms(g.latest)) g.latest = u.last_at;
	}
	for (const i of ice) {
		const g = group(`ice:${i.profile.id}`, null, i.profile, i.last_at);
		g.uses = i.count;
		g.lastUse = i.last_at;
		g.unread += i.unread;
		if (ms(i.last_at) > ms(g.latest)) g.latest = i.last_at;
	}
	for (const g of groups.values()) g.steps.sort((a, b) => ms(a.at) - ms(b.at));
	return [...groups.values()].sort((a, b) => ms(b.latest) - ms(a.latest));
}

/** The badge: how many requests have something unread. */
export const unreadGroups = (groups: readonly NoticeGroup[]): number => groups.filter((g) => g.unread > 0).length;

/** What the notices panel shows: the last answer, the last error, and whether it is open. */
export interface NoticeView {
	data: Notices | null;
	error: unknown;
	open: boolean;
}

/**
 * The notices' load, kept free of runes so the tests can run it. An answer counts only when it is the
 * newest load and the person it was read for is still the one signed in: a load that started before
 * Mark all as read, or for the last person, and ends late is dropped. After every answer that counts,
 * unread notices open the panel once per login: `store` keeps the user id of the login they opened
 * for (NOTICES_SHOWN), so a first load that failed opens them at the next one that works.
 */
export function noticeLoader(
	v: NoticeView,
	user: () => string,
	fetch: () => Promise<Notices>,
	store: Pick<Storage, 'getItem' | 'setItem'>
): () => Promise<void> {
	let seq = 0;
	return async () => {
		const n = ++seq;
		const u = user();
		if (!u) return;
		const current = () => n === seq && u === user();
		try {
			const d = await fetch();
			if (!current()) return;
			v.data = d;
			v.error = null;
			if (!unreadGroups(groupNotices(d.items, d.uses, d.ice)) || store.getItem(NOTICES_SHOWN) === u) return;
			store.setItem(NOTICES_SHOWN, u);
			v.open = true;
		} catch (e) {
			if (current()) v.error = e;
		}
	};
}

type Member = {
	user_id: string;
	display_name: string;
	own_profile?: string | null;
	enrolled?: boolean;
	logins?: { passkeys: number; oidc: boolean; password: boolean };
};
type Grant = { user_id: string; profile: string; role: string };

/**
 * Who the owner may name on `slug`: the household's logins, except the owner themself, anyone who
 * already owns the profile (the server refuses them too) and those named already.
 */
export function contactChoices<M extends Member>(members: readonly M[], grants: readonly Grant[], slug: string, me: string, named: readonly string[]): M[] {
	const owns = (m: Member) => m.own_profile === slug || grants.some((g) => g.user_id === m.user_id && g.profile === slug && g.role === 'owner');
	return members.filter((m) => m.user_id !== me && !owns(m) && !named.includes(m.user_id));
}

/**
 * Whether a member can log in, and so ask: a passkey, a password or OIDC. An invite not redeemed yet
 * cannot. Without the login details (a reader who may not see them), an enrolled member counts.
 */
export function canLogIn(m: Member): boolean {
	if (m.logins) return m.logins.passkeys > 0 || m.logins.oidc || m.logins.password;
	return m.enrolled !== false;
}
