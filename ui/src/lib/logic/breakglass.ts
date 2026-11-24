// Break-glass on screen (routes/break_glass.rs): where a request stands, the notices grouped per
// request, and who the owner may name. Pure, so the tests can run it without a browser.
import type { BreakGlassRequest, Notice, NoticeKind } from '../api/types';

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

/** A date and time in the reader's language and zone: '26 nov 2026, 10:00'. */
export function when(iso: string | null | undefined, locale: string): string {
	if (!iso) return '—';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return '—';
	return new Intl.DateTimeFormat(locale, { dateStyle: 'medium', timeStyle: 'short' }).format(d);
}

/** The notices of one request, as one entry of the outbox. */
export interface NoticeGroup {
	/** the request id (a notice without one stands alone, under its own id) */
	key: string;
	request: string | null;
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
	unread: string[];
}

/**
 * The outbox, one entry per request, newest first. Every read through an open request writes a
 * notice, so two page views can write twenty; they show as one line with the count.
 */
export function groupNotices(items: readonly Notice[]): NoticeGroup[] {
	const groups = new Map<string, NoticeGroup>();
	for (const n of items) {
		const key = n.request_id ?? n.id;
		let g = groups.get(key);
		if (!g) {
			g = { key, request: n.request_id, profile: n.profile, contact: null, steps: [], uses: 0, lastUse: null, latest: n.created_at, unread: [] };
			groups.set(key, g);
		}
		g.contact ??= n.contact?.display_name ?? null;
		if (n.created_at > g.latest) g.latest = n.created_at;
		if (!n.read_at) g.unread.push(n.id);
		if (n.kind === 'break_glass.use') {
			g.uses += 1;
			if (!g.lastUse || n.created_at > g.lastUse) g.lastUse = n.created_at;
		} else {
			g.steps.push({ kind: n.kind, at: n.created_at, by: n.actor?.display_name ?? null, unread: !n.read_at });
		}
	}
	for (const g of groups.values()) g.steps.sort((a, b) => (a.at < b.at ? -1 : a.at > b.at ? 1 : 0));
	return [...groups.values()].sort((a, b) => (a.latest < b.latest ? 1 : a.latest > b.latest ? -1 : 0));
}

/** The badge: how many requests have something unread. */
export const unreadGroups = (groups: readonly NoticeGroup[]): number => groups.filter((g) => g.unread.length > 0).length;

type Member = { user_id: string; display_name: string; own_profile?: string | null };
type Grant = { user_id: string; profile: string; role: string };

/**
 * Who the owner may name on `slug`: the household's logins, except the owner themself, anyone who
 * already owns the profile (the server refuses them too) and those named already.
 */
export function contactChoices<M extends Member>(members: readonly M[], grants: readonly Grant[], slug: string, me: string, named: readonly string[]): M[] {
	const owns = (m: Member) => m.own_profile === slug || grants.some((g) => g.user_id === m.user_id && g.profile === slug && g.role === 'owner');
	return members.filter((m) => m.user_id !== me && !owns(m) && !named.includes(m.user_id));
}

/** The request `id` among the owner views of several profiles, with the profile it belongs to. */
export function findRequest<R extends { id: string }>(views: readonly { slug: string; open_requests: readonly R[]; requests: readonly R[] }[], id: string): { slug: string; request: R } | null {
	for (const v of views) {
		const r = v.open_requests.find((x) => x.id === id) ?? v.requests.find((x) => x.id === id);
		if (r) return { slug: v.slug, request: r };
	}
	return null;
}
