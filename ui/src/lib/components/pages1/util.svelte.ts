// Small helpers shared by the lane H-ui-1 pages (dashboard, biomarkers, timeline, wearables, imaging, family).
import { ApiError } from '#lib/api/core';
import { i18n, t } from '#lib/i18n/index.svelte';
import { dict } from '#lib/i18n/dict';
import { pluralKey } from '#lib/logic/plural';

/** t() with {name} placeholders. */
export function tf(key: string, vars: Record<string, string | number>): string {
	return t(key).replace(/\{(\w+)\}/g, (_, k) => (k in vars ? String(vars[k]) : `{${k}}`));
}

/** tf() for a count: uses `key_one` for one ("1 day missing", "falta 1 día"). `n` is also a placeholder. */
export function tp(key: string, n: number, vars: Record<string, string | number> = {}): string {
	return tf(pluralKey(i18n.lang, n, key, (k) => k in dict.en), { n, ...vars });
}

/**
 * One fetch on screen: data, error, loading. `run` ignores answers that arrive after a newer call,
 * so switching profile or language fast never shows the previous profile's data.
 */
export class Loader<T> {
	data = $state<T | null>(null);
	error = $state<ApiError | Error | null>(null);
	loading = $state(false);
	#seq = 0;

	async run(fn: () => Promise<T>): Promise<void> {
		const seq = ++this.#seq;
		this.loading = true;
		this.error = null;
		try {
			const d = await fn();
			if (seq === this.#seq) this.data = d;
		} catch (e) {
			if (seq === this.#seq) {
				this.data = null;
				this.error = e instanceof Error ? e : new Error(String(e));
			}
		} finally {
			if (seq === this.#seq) this.loading = false;
		}
	}
}

/** Short code for an error shown in an empty state: 'HTTP 404', 'HTTP 500 · …'. */
export function errCode(e: unknown): string {
	if (e instanceof ApiError) return `HTTP ${e.status}${e.code && !e.code.startsWith('http_') ? ' · ' + e.code : ''}`;
	return e instanceof Error ? e.message : String(e);
}

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
