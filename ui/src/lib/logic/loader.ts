/** What a Loader shows: the last answer, the last error, and whether a call is in flight. */
export interface LoadState<T> {
	data: T | null;
	error: Error | null;
	loading: boolean;
}

/**
 * The run logic behind Loader, kept free of runes so it can be tested. An answer that arrives after a
 * newer call is dropped. A call with a `key` (the profile, the drug, the source id) different from the
 * previous call's clears `data` at once, so the old profile's data never sits on screen under the new
 * one while the request runs. A call without a key, or with the same key, keeps `data` (a reload).
 */
export function latestRunner<T>(s: LoadState<T>): (fn: () => Promise<T>, key?: string) => Promise<void> {
	let seq = 0;
	let last: string | undefined;
	return async (fn, key) => {
		const n = ++seq;
		if (key !== undefined) {
			if (key !== last) s.data = null;
			last = key;
		}
		s.loading = true;
		s.error = null;
		try {
			const d = await fn();
			if (n === seq) s.data = d;
		} catch (e) {
			if (n === seq) {
				s.data = null;
				s.error = e instanceof Error ? e : new Error(String(e));
			}
		} finally {
			if (n === seq) s.loading = false;
		}
	};
}
