<script lang="ts">
	// Port of the mockup's spark(): grey line, last point in the status colour, optional range band.
	import { C } from './colors';
	import type { Status } from '#lib/api/types';

	let { values, status = 'ok', w = 74, h = 24, band = null }: {
		values: number[];
		status?: Status | 'accent';
		w?: number;
		h?: number;
		band?: [number, number] | null;
	} = $props();

	const geo = $derived.by(() => {
		const vals = values.length ? values : [0];
		const mn = Math.min(...vals, band ? band[0] : Infinity);
		const mx = Math.max(...vals, band ? band[1] : -Infinity);
		const sx = (i: number) => 2 + (i * (w - 6)) / Math.max(1, vals.length - 1);
		const sy = (v: number) => h - 3 - ((v - mn) / (mx - mn || 1)) * (h - 6);
		const pts = vals.map((v, i) => `${sx(i).toFixed(1)},${sy(v).toFixed(1)}`).join(' ');
		let rect = null as null | { y: number; height: number };
		if (band) {
			const y1 = sy(Math.min(band[1], mx));
			const y0 = sy(Math.max(band[0], mn));
			rect = { y: y1, height: Math.max(0, y0 - y1) };
		}
		return { pts, rect, cx: sx(vals.length - 1), cy: sy(vals[vals.length - 1]) };
	});
	const col = $derived((C as Record<string, string>)[status] ?? C.accent);
</script>

<svg width={w} height={h} viewBox="0 0 {w} {h}" aria-hidden="true">
	{#if geo.rect}<rect x="0" y={geo.rect.y} width={w} height={geo.rect.height} fill="#3fb68b" opacity=".09" />{/if}
	{#if values.length > 1}<polyline points={geo.pts} fill="none" stroke={C.line} stroke-width="1.3" stroke-linejoin="round" />{/if}
	{#if values.length}<circle cx={geo.cx} cy={geo.cy} r="2.8" fill={col} />{/if}
</svg>
