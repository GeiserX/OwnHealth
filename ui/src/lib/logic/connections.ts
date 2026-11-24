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
	/** The caller owns or guards the profile, so may reconnect or disconnect it. Absent for a source token. */
	can_manage?: boolean;
	// The fields below come only with can_manage true (routes/inboxes.rs); the screen lists only those rows.
	/** When the source was made, and its poll interval: with no run yet, it has no data until one interval passed. */
	created_at?: string;
	poll_seconds?: number | null;
	/** The vendor refused the token: the person must sign in again. */
	reconsent_needed_at?: string | null;
	/** When the person's consent at the vendor ends. */
	grant_expires_at?: string | null;
	/** When the current access token ends (it is refreshed by itself before that). */
	access_expires_at?: string | null;
	/** Open sync-gap alerts of this source on its own profile, in the review queue. */
	open_gaps?: number;
	/** The server the connection talks to (SMART: the FHIR base URL); null for an inbox. */
	server?: string | null;
	/** Distinct Medicare claims received in the latest claim pull. */
	claim_count?: number;
}

export type ConnectionState = 'reconsent' | 'expired' | 'grant_soon' | 'off' | 'out' | 'partial' | 'stale' | 'nodata' | 'ok';
export type StatusWord = 'ok' | 'watch' | 'out' | 'stale' | 'nodata' | 'off';

const DAY = 86_400_000;

/** Whole days from `now` to `iso`, rounded up (a grant ending in 2.1 days has 3 days left); null without a date. */
export function daysLeft(iso: string | null | undefined, now = new Date()): number | null {
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

/** The status word each state shows (information, never advice: OK, Watch, Out, Stale, No data, Off as on the Inbox screen). */
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
			return 'off';
		case 'nodata':
			return 'nodata';
		default:
			return 'ok';
	}
}

/** A state that only a new sign-in at the vendor fixes. */
export const wantsReconnect = (s: ConnectionState): boolean => s === 'reconsent' || s === 'expired' || s === 'grant_soon';

/** The access token is renewed by itself only while the vendor still accepts the connection. */
export const renewsItself = (s: ConnectionState): boolean => s !== 'reconsent' && s !== 'expired' && s !== 'off';

/** The vendor has a sign-in to send the person back to, and the person manages the profile. */
export const canReconnect = (r: ConnectionRow): boolean => OAUTH_KINDS.includes(r.kind) && r.can_manage === true;

/** A card's title. The provider directory is a source the app makes, so its title is in the reader's
 * language; every other connection keeps the label it was given. */
export const cardTitle = (r: Pick<ConnectionRow, 'kind' | 'label'>, t: (k: string) => string): string =>
	r.kind === 'brands' ? t('con_t_brands') : r.label;

/** The rows the screen lists: connections (not inboxes) of profiles the person manages. */
export function shownConnections<T extends ConnectionRow>(rows: T[]): T[] {
	return rows.filter((r) => (CONNECTOR_KINDS as readonly string[]).includes(r.kind) && r.can_manage === true);
}

/** After a connect, how often the screen reads the rows again while the first pull runs, and how many times at most (3 minutes). */
export const FIRST_PULL_EVERY = 5_000;
export const FIRST_PULL_TRIES = 36;

/** The first pull of the row's current grant has not finished yet (last_run_status covers only that grant's runs). */
export function firstPullPending(r: ConnectionRow | undefined): boolean {
	return !!r && r.enabled && !r.reconsent_needed_at && r.last_run_status == null;
}

/**
 * After a connect or reconnect: read the rows again every FIRST_PULL_EVERY ms while the first pull of
 * connection `id` has not finished, at most `tries` times, and never once `alive()` is false (the
 * screen was left). `load` reads the rows; `wait` is a timer.
 */
export async function refreshUntilFirstPull(
	load: () => Promise<ConnectionRow[] | null | undefined>,
	id: string,
	wait: (ms: number) => Promise<void>,
	alive: () => boolean,
	tries = FIRST_PULL_TRIES
): Promise<void> {
	for (let i = 0; i < tries; i++) {
		await wait(FIRST_PULL_EVERY);
		if (!alive()) return;
		const list = await load();
		if (!alive() || !firstPullPending(list?.find((r) => r.id === id))) return;
	}
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

/**
 * The words a person reads for a poller's stop or problem text, which the server keeps in English
 * with its code in front ('JobError: no_answer: …', 'undeclared_host: MedicationRequest: …'):
 * `con_run_<code>` (or the callback's `con_err_<code>`), after the FHIR type it names, with the first
 * number of the text as `{n}` and `(3 times)` said again. Null when the code has no words, so the
 * screen shows the text as sent. `say` is tf().
 */
export function runWords(
	text: string,
	has: (k: string) => boolean,
	say: (key: string, vars: Record<string, string | number>) => string
): string | null {
	const m = /^\s*(?:\w*(?:Error|Exception):\s*)?([a-z][a-z0-9_]*): ([\s\S]*)$/.exec(text);
	if (!m) return null;
	const [, code, rest] = m;
	const key = has('con_run_' + code) ? 'con_run_' + code : code !== 'unknown' && has('con_err_' + code) ? 'con_err_' + code : null;
	if (!key) return null;
	const type = /^([A-Z][A-Za-z]+): /.exec(rest);
	const body = type ? rest.slice(type[0].length) : rest;
	const n = /\d+/.exec(body)?.[0] ?? '';
	const times = /\((\d+) times\)$/.exec(body)?.[1];
	let words = say(key, { n });
	if (type) words = `${type[1]}: ${words.charAt(0).toLocaleLowerCase()}${words.slice(1)}`;
	return times ? `${words} ${say('con_run_times', { n: Number(times) })}` : words;
}
