// The emergency card form (routes/summary/IceCard.svelte): the expiry it previews, and the clock that
// moves that preview on at 00:00 UTC. Free of runes, so the tests can run it.

/**
 * The expiry the server sets (routes/ice.rs): 00:00 UTC, `days` after the UTC date of `now` (ms).
 * Empty when `days` is not a whole number from 1 to 365, which the server refuses.
 */
export function iceUntil(days: unknown, now: number): string {
	const n = Math.trunc(Number(days));
	if (!Number.isFinite(n) || n < 1 || n > 365) return '';
	const d = new Date(now);
	return new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate() + n)).toISOString();
}

type Clock = { now: () => number; set: (f: () => void, ms: number) => unknown; clear: (h: unknown) => void };
const system: Clock = {
	now: () => Date.now(),
	set: (f, ms) => setTimeout(f, ms),
	clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>)
};

/**
 * Calls `onday` at each 00:00 UTC, so a page left open past it previews the date the server will use.
 * Returns the stop; call it on unmount. A sleeping tab can run the timer late, so the page also reads
 * the clock again when the tab comes back.
 */
export function utcMidnights(onday: () => void, clock: Clock = system): () => void {
	let timer: unknown;
	let stopped = false;
	const arm = () => {
		const d = new Date(clock.now());
		const next = Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate() + 1);
		timer = clock.set(() => {
			if (stopped) return;
			onday();
			arm();
		}, next - d.getTime());
	};
	arm();
	return () => {
		stopped = true;
		if (timer !== undefined) clock.clear(timer);
	};
}
