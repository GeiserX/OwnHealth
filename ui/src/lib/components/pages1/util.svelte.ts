// Small helpers shared by the lane H-ui-1 pages (dashboard, biomarkers, timeline, wearables, imaging, family).
import { ApiError } from '#lib/api/core';
import { i18n, t } from '#lib/i18n/index.svelte';
import { dict } from '#lib/i18n/dict';
import { latestRunner } from '#lib/logic/loader';
import { pluralKey } from '#lib/logic/plural';
import { errKey, stripErrorName } from '#lib/logic/errors';

export { unitLabel } from '#lib/logic/format';

/** t() with {name} placeholders. */
export function tf(key: string, vars: Record<string, string | number>): string {
	return t(key).replace(/\{(\w+)\}/g, (_, k) => (k in vars ? String(vars[k]) : `{${k}}`));
}

/** tf() for a count: uses `key_one` for one ("1 day missing", "falta 1 día"). `n` is also a placeholder. */
export function tp(key: string, n: number, vars: Record<string, string | number> = {}): string {
	return tf(pluralKey(i18n.lang, n, key, (k) => k in dict.en), { n, ...vars });
}

/**
 * One fetch on screen: data, error, loading. `run` ignores answers that arrive after a newer call, and
 * a call with a new `key` (pass the profile slug, the drug, the id) clears the previous data at once,
 * so switching profile never shows the previous profile's data. Logic in #lib/logic/loader.
 */
export class Loader<T> {
	data = $state<T | null>(null);
	error = $state<ApiError | Error | null>(null);
	loading = $state(false);
	#run = latestRunner<T>(this);

	run(fn: () => Promise<T>, key?: string): Promise<void> {
		return this.#run(fn, key);
	}
}

/** Short code for an error shown in an empty state: 'HTTP 404', 'HTTP 500 · …', 'no connection'. */
export function errCode(e: unknown): string {
	if (e instanceof ApiError) return `HTTP ${e.status}${e.code && !e.code.startsWith('http_') ? ' · ' + e.code : ''}`;
	const m = e instanceof Error ? e.message : String(e);
	return errKey(null, m, (k) => k in dict.en) === 'err_no_connection' ? t('err_conn_short') : m;
}

/** The raw text of an error, for a title or a dim second line under errText(). */
export function errRaw(e: unknown): string {
	return e instanceof Error ? e.message : String(e);
}

/**
 * What a person reads for an error: our sentence for the server's code or for a common cause (connection
 * refused, DNS, TLS, a duplicate, an unsupported file…), else the server's own message. Logic in #lib/logic/errors.
 */
export function errText(e: unknown): string {
	const code = e instanceof ApiError ? e.code : null;
	const raw = errRaw(e);
	const key = errKey(code, raw, (k) => k in dict.en);
	if (key) return t(key);
	return e instanceof ApiError && !raw ? `HTTP ${e.status}` : stripErrorName(raw);
}

/** A scope in plain words ('Read lab results'), or the scope id when there are none. */
export function scopeText(scope: string): string {
	const k = 'fu_sc_' + scope.replace(':', '_');
	const w = t(k);
	return w === k ? scope : w;
}

/** A label that starts a line or an option: 'Hija o hijo' from 'hija o hijo'. */
export const cap = (s: string): string => (s ? s[0].toLocaleUpperCase() + s.slice(1) : s);

/** Decimal year of an ISO date, for time axes (same formula as the mockup's yfrac). */
export function yfrac(d: string): number {
	const [y, m, dd] = d.slice(0, 10).split('-').map(Number);
	return y + (m - 1) / 12 + ((dd || 15) - 1) / 365;
}

/** 'sha256:4be1…' from a full hash. */
export function shortHash(h: string): string {
	const [algo, hex] = h.includes(':') ? h.split(':', 2) : ['sha256', h];
	return `${algo}:${hex.slice(0, 4)}…`;
}
