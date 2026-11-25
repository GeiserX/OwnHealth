// The share-links page (/links): who may share a profile, what a link carries, the words for its
// state and its access log, and the QR drawing. Pure, so tests/links.test.ts runs it under node.
import type { ProfileSummary, Role, ShlAccess, ShlCategory, ShlLink } from '#lib/api/types';

/** The roles routes/shl.rs lets share (`ROLES`). A break-glass grant is refused there too. */
export const SHARER_ROLES: readonly Role[] = ['owner', 'guardian'];

/** Whether this grant may share the profile: owner or guardian, and not a break-glass grant. */
export const mayShare = (role: Role | null | undefined, breakGlass?: boolean | null): boolean => !!role && SHARER_ROLES.includes(role) && !breakGlass;

export const shareLinksHref = (profileId: string): string => `/links?profile=${encodeURIComponent(profileId)}`;

export function profileForLinks(
	profileId: string | null,
	profiles: readonly Pick<ProfileSummary, 'id' | 'slug' | 'role' | 'break_glass'>[],
	current: string | null
): string | null {
	const target = profiles.find((p) => p.id === profileId && mayShare(p.role, p.break_glass));
	return target?.slug ?? current;
}

/** Whether the views menu shows Share links: at least one profile this person may share. */
export const sharesAny = (profiles: readonly { role: Role; break_glass?: boolean }[] | null | undefined): boolean =>
	(profiles ?? []).some((p) => mayShare(p.role, p.break_glass));

/** The categories a person picks. read:summary comes with every link (routes/shl.rs create). */
export const PICKS: readonly ShlCategory[] = ['read:labs', 'read:records'];

/**
 * The body's categories: the summary plus each pick the server says a link of this person may carry
 * (`open` of GET /profiles/{p}/shl: their categories minus the profile's locks, the owner's too).
 */
export function categoriesFor(picked: readonly string[], open: readonly string[]): ShlCategory[] {
	return ['read:summary', ...PICKS.filter((c) => picked.includes(c) && open.includes(c))];
}

/** Whether any link can be made: every link carries the summary, so it must be open. */
export const canShare = (open: readonly string[]): boolean => open.includes('read:summary');

/** A link's categories in the order the form lists them, the summary first. */
export function shownCategories(cats: readonly string[]): ShlCategory[] {
	return (['read:summary', ...PICKS] as ShlCategory[]).filter((c) => cats.includes(c));
}

/** Expiry choices in days; the server takes 1 to 365 and defaults to 7. */
export const DAYS: readonly number[] = [1, 7, 30, 90];
export const DEFAULT_DAYS = 7;

/** routes/shl.rs PASSCODE_MIN and PASSCODE_MAX, counted in characters. */
export const PIN_MIN = 4;
export const PIN_MAX = 64;

/** The dict key of what is wrong with a typed PIN, or null. An empty PIN means no PIN. */
export function pinProblem(pin: string): string | null {
	const n = [...pin].length;
	return n === 0 || (n >= PIN_MIN && n <= PIN_MAX) ? null : 'shl_pin_len';
}

/**
 * A link's state at `now`: the server's word, until the expiry passes on a page left open. Then a live
 * or locked-out link has expired, as routes/shl.rs `state` would say now; a revoked one stays revoked.
 */
export function stateAt(link: Pick<ShlLink, 'state' | 'reason' | 'expires_at'>, now: number): Pick<ShlLink, 'state' | 'reason'> {
	return link.state !== 'revoked' && Date.parse(link.expires_at) <= now ? { state: 'expired', reason: null } : link;
}

/** The words and the colour of a link's state at `now`. */
export function stateOf(link: Pick<ShlLink, 'state' | 'reason' | 'expires_at'>, now: number): { key: string; why: string | null; tone: string } {
	const { state, reason } = stateAt(link, now);
	const tone = state === 'active' ? 'p-ok' : state === 'paused' ? 'p-watch' : 'p-out';
	return { key: 'shl_st_' + state, why: reason ? 'shl_why_' + reason : null, tone };
}

/** The word before a link's expiry date: past dates read as past, whatever else ended the link. */
export const expiryKey = (link: Pick<ShlLink, 'expires_at'>, now: number): string =>
	Date.parse(link.expires_at) <= now ? 'shl_expired_on' : 'shl_expires';

/** Whether a Revoke button still does something at `now`: a revoked, expired or locked-out link is over. */
export function revocable(link: Pick<ShlLink, 'state' | 'expires_at'>, now: number): boolean {
	const { state } = stateAt({ ...link, reason: null }, now);
	return state === 'active' || state === 'paused';
}

/** The words of one access log row. */
export const accessKey = (kind: ShlAccess['kind']): string => 'shl_ax_' + kind;

/**
 * An SVG path that paints every dark module of a QR code of `size` x `size` modules, one rectangle per
 * run of dark modules in a row, offset by a quiet zone of `quiet` modules.
 */
export function qrPath(size: number, dark: (x: number, y: number) => boolean, quiet = 4): string {
	let d = '';
	for (let y = 0; y < size; y++) {
		let x = 0;
		while (x < size) {
			if (!dark(x, y)) {
				x++;
				continue;
			}
			const start = x;
			while (x < size && dark(x, y)) x++;
			d += `M${start + quiet},${y + quiet}h${x - start}v1h-${x - start}z`;
		}
	}
	return d;
}
