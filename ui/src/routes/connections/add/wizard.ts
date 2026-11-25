// The add-connection wizard (slice 19): what it shows for one vendor profile of
// data/connectors/connectors.yaml (GET /api/connectors/profiles), which saved client a server uses,
// and what the launch sends. No browser and no Svelte here, so tests/connector_wizard.test.ts runs it.

import type { SavedClient, VendorProfile } from '#lib/api/types';

export type { SavedClient, VendorProfile };

/** A server address as the server stores it: trimmed, no trailing slash. */
export const bare = (url: string): string => url.trim().replace(/\/+$/, '');

/** The scopes the vendor app must allow, as one line to paste into the vendor's form. */
export const scopeLine = (v: Pick<VendorProfile, 'scopes'>): string => v.scopes.join(' ');

/** A confidential vendor's client carries a secret; a public one never does. */
export const needsSecret = (v: Pick<VendorProfile, 'client_type'>): boolean => v.client_type === 'confidential';

/** Where the person picks the server: the US directory search, or the vendor's own list. */
export const serverPick = (v: Pick<VendorProfile, 'directory'>): 'directory' | 'list' => (v.directory ? 'directory' : 'list');

/**
 * The saved client the launch for this vendor and server uses: the one registered for that server,
 * else, for a public vendor, one saved for every server (issuer null). Null when none is saved yet.
 */
export function clientFor(clients: SavedClient[], v: Pick<VendorProfile, 'key' | 'client_type'>, server: string): SavedClient | null {
	const s = bare(server);
	const mine = clients.filter((c) => c.vendor === v.key);
	return (
		mine.find((c) => c.issuer != null && bare(c.issuer) === s) ??
		(v.client_type === 'public' ? (mine.find((c) => c.issuer == null) ?? null) : null)
	);
}

/**
 * The issuer the save sends: the chosen server for a confidential vendor (its secret goes to that
 * server only), none for a public vendor (its client id serves every server of the vendor).
 */
export const saveIssuer = (v: Pick<VendorProfile, 'client_type'>, server: string): string | undefined =>
	v.client_type === 'confidential' ? bare(server) : undefined;

/** Directory hits of this vendor only: the US directory lists every vendor's facilities. */
export function hitsOf<T extends { vendor: string }>(hits: T[], v: Pick<VendorProfile, 'key'>): T[] {
	return hits.filter((h) => h.vendor === v.key);
}

/** A server address the person typed, once it looks like a web address; else null. */
export function typedServer(text: string): string | null {
	const t = bare(text);
	try {
		const u = new URL(t);
		return (u.protocol === 'https:' || u.protocol === 'http:') && u.host ? t : null;
	} catch {
		return null;
	}
}

/** The name a new connection gets when the person types none: the facility, else the server's host. */
export function defaultLabel(server: string, facility: string | null): string {
	if (facility?.trim()) return facility.trim().slice(0, 120);
	try {
		return new URL(server).hostname;
	} catch {
		return '';
	}
}

/**
 * The facts of a vendor's tokens, as dictionary keys with their numbers: how a refresh token is
 * renewed, when an unused sign-in ends, how long an access token lasts. Unknown values say so.
 */
export function tokenFacts(v: Pick<VendorProfile, 'refresh' | 'access_token_seconds'>): { key: string; vars: Record<string, number> }[] {
	const out: { key: string; vars: Record<string, number> }[] = [{ key: 'cw_rot_' + v.refresh.rotation, vars: {} }];
	const days = v.refresh.unused_expiry_days;
	out.push(days ? { key: 'cw_unused_days', vars: { n: days } } : { key: 'cw_unused_unknown', vars: {} });
	const secs = v.access_token_seconds;
	if (secs) out.push(secs % 60 === 0 ? { key: 'cw_access_min', vars: { n: secs / 60 } } : { key: 'cw_access_sec', vars: { n: secs } });
	return out;
}
