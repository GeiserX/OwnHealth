// Pure helpers for the Connections screen (slice 55). No Svelte, no fetch: tests/connections.test.ts
// runs them under node. The rows are GET /api/inboxes/health; the routes are routes/connectors.rs.
import { sourceStatus } from './inbox.ts';

/** The source kinds that are connections, not inboxes (core::sources::CONNECTOR_KINDS). */
export const CONNECTOR_KINDS = ['smart', 'bluebutton', 'dexcom', 'openwearables', 'brands'] as const;
export type ConnectorKind = (typeof CONNECTOR_KINDS)[number];
/** The kinds that sign in at the vendor, so the screen can send the person back there (connector_oauth::OAUTH_KINDS). */
export const OAUTH_KINDS: readonly string[] = ['smart', 'bluebutton', 'dexcom'];
/** A grant this close to its end is shown as ending soon, with a Reconnect button. */
export const GRANT_SOON_DAYS = 7;

/** One row of GET /api/inboxes/health, with the connection fields. */
export interface ConnectionRow {
	id: string;
	kind: string;
	label: string;
	profile_id: string | null;
	enabled: boolean;
	last_run_at: string | null;
	last_success_at: string | null;
	last_error_at: string | null;
	last_error: string | null;
	consecutive_failures: number;
	stale: boolean;
	last_run_status: 'ok' | 'partial' | 'error' | null;
	last_run_problems: number;
	last_run_problem: string | null;
	/** The vendor refused the token: the person must sign in again. */
	reconsent_needed_at: string | null;
	/** When the person's consent at the vendor ends. */
	grant_expires_at: string | null;
	/** When the current access token ends (it is refreshed by itself before that). */
	access_expires_at: string | null;
	/** Open sync-gap alerts of this source in the review queue. */
	open_gaps: number;
	/** The server the connection talks to (SMART: the FHIR base URL); null for an inbox. */
	server: string | null;
	/** The caller owns or guards the profile, so may reconnect or disconnect it. Absent for a source token. */
	can_manage?: boolean;
}

export type ConnectionState = 'reconsent' | 'expired' | 'grant_soon' | 'off' | 'out' | 'partial' | 'stale' | 'nodata' | 'ok';
export type StatusWord = 'ok' | 'watch' | 'out' | 'stale' | 'nodata';

const DAY = 86_400_000;

/** Whole days from `now` to `iso`, rounded up (a grant ending in 2.1 days has 3 days left); null without a date. */
export function daysLeft(iso: string | null, now = new Date()): number | null {
	if (!iso) return null;
	return Math.ceil((new Date(iso).getTime() - now.getTime()) / DAY);
}

/** What one connection needs: consent again first, then the grant, then how its runs went. */
export function connectionState(r: ConnectionRow, now = new Date()): ConnectionState {
	if (r.reconsent_needed_at) return 'reconsent';
	if (r.grant_expires_at) {
		const left = new Date(r.grant_expires_at).getTime() - now.getTime();
		if (left <= 0) return 'expired';
		if (left <= GRANT_SOON_DAYS * DAY) return 'grant_soon';
	}
	return sourceStatus(r, now);
}

/** The status word each state shows (information, never advice: OK, Watch, Out, Stale, No data). */
export function statusWord(s: ConnectionState): StatusWord {
	switch (s) {
		case 'reconsent':
		case 'expired':
		case 'out':
			return 'out';
		case 'grant_soon':
		case 'partial':
			return 'watch';
		case 'stale':
			return 'stale';
		case 'off':
		case 'nodata':
			return 'nodata';
		default:
			return 'ok';
	}
}

/** A state that only a new sign-in at the vendor fixes. */
export const wantsReconnect = (s: ConnectionState): boolean => s === 'reconsent' || s === 'expired' || s === 'grant_soon';

/** The vendor has a sign-in to send the person back to, and the person manages the profile. */
export const canReconnect = (r: ConnectionRow): boolean => OAUTH_KINDS.includes(r.kind) && r.can_manage === true;

/** The rows the screen lists: connections (not inboxes) of profiles the person manages. */
export function shownConnections<T extends ConnectionRow>(rows: T[]): T[] {
	return rows.filter((r) => (CONNECTOR_KINDS as readonly string[]).includes(r.kind) && r.can_manage === true);
}

/** Where the connector callback sent the browser back: `?connected=<id>` or `?connector_error=<code>`. */
export function callbackOutcome(q: Pick<URLSearchParams, 'get'>): { connected: string } | { error: string } | null {
	const id = q.get('connected');
	if (id) return { connected: id };
	const code = q.get('connector_error');
	if (code == null) return null;
	// our codes are short lower-case words; anything else is not put on the screen
	return { error: /^[a-z_]{1,40}$/.test(code) ? code : 'unknown' };
}

/** The i18n key for a connector code, or the generic one for a code with no words yet. */
export function connectorErrorKey(code: string, has: (k: string) => boolean): string {
	return has('con_err_' + code) ? 'con_err_' + code : 'con_err_unknown';
}
