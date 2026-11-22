// The bounds of a sparkline (Sparkline.svelte), apart so the tests can run them without a renderer.

/** The lowest and highest value of `vals`, widened to the range band when there is one. A reduce, not a
 * spread: a long series (a year of intraday scores) passes the engine's limit on call arguments. */
export function sparkBounds(vals: number[], band: [number, number] | null): [number, number] {
	const mn = vals.reduce((a, v) => Math.min(a, v), band ? band[0] : Infinity);
	const mx = vals.reduce((a, v) => Math.max(a, v), band ? band[1] : -Infinity);
	return [mn, mx];
}
