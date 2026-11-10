<script lang="ts" module>
	export interface Series {
		vals: (number | null)[];
		color: string;
		sw?: number;
		dash?: boolean;
		dots?: boolean;
		op?: number;
		colorFn?: (v: number) => string;
	}
</script>

<script lang="ts">
	// Port of the mockup's tsChart(): time series or bars with grid, optional range band and reference line.
	// Points get an `onpoint` callback (index, event) so pages can show the printed-value tooltip.
	import { C } from './colors';

	let {
		series,
		yMin,
		yMax,
		w = 380,
		h = 205,
		band = null,
		ticks = [],
		xlabels = [],
		bars = false,
		fmt = (v: number) => String(v),
		refLine = null,
		pad = [8, 8, 20, 32],
		label,
		onpoint
	}: {
		series: Series[];
		yMin: number;
		yMax: number;
		w?: number;
		h?: number;
		band?: [number, number] | null;
		ticks?: number[];
		xlabels?: [number, string][];
		bars?: boolean;
		fmt?: (v: number) => string;
		refLine?: number | null;
		pad?: [number, number, number, number];
		/** What the chart shows, for screen readers ('Steps, average 8 400, 30 days'). */
		label?: string;
		onpoint?: (index: number, event: MouseEvent | null) => void;
	} = $props();

	const g = $derived.by(() => {
		const [pt, pr, pb, pl] = pad;
		const iw = w - pl - pr;
		const ih = h - pt - pb;
		const n = Math.max(1, ...series.map((s) => s.vals.length));
		const sx = (i: number) => pl + (bars ? ((i + 0.5) * iw) / n : (i * iw) / Math.max(1, n - 1));
		const sy = (v: number) => pt + ih - ((v - yMin) / (yMax - yMin || 1)) * ih;
		return { pt, pr, pb, pl, iw, ih, n, sx, sy };
	});

	function path(s: Series): string {
		let d = '';
		let pen = false;
		s.vals.forEach((v, i) => {
			if (v == null) {
				pen = false;
				return;
			}
			d += (pen ? 'L' : 'M') + g.sx(i).toFixed(1) + ',' + g.sy(v).toFixed(1);
			pen = true;
		});
		return d;
	}
	// The last label ends at its point so it never runs past the right edge, bars included.
	const anchor = (k: number, len: number) => (k === len - 1 ? 'end' : bars ? 'middle' : k === 0 ? 'start' : 'middle');
</script>

<svg width="100%" viewBox="0 0 {w} {h}" preserveAspectRatio="xMidYMid meet" role="img" aria-label={label}>
	{#each ticks as v (v)}
		<line x1={g.pl} x2={w - g.pr} y1={g.sy(v)} y2={g.sy(v)} stroke={C.grid} />
		<text x={g.pl - 5} y={g.sy(v) + 3.5} fill={C.axis} font-size="10" text-anchor="end">{fmt(v)}</text>
	{/each}
	{#if band}
		<rect x={g.pl} y={g.sy(band[1])} width={g.iw} height={g.sy(band[0]) - g.sy(band[1])} fill="#3fb68b" opacity=".08" />
	{/if}
	{#if refLine != null}
		<line x1={g.pl} x2={w - g.pr} y1={g.sy(refLine)} y2={g.sy(refLine)} stroke={C.accent} stroke-dasharray="4 3" opacity=".7" />
	{/if}
	{#each xlabels as [i, l], k (k)}
		<text x={g.sx(i)} y={h - 5} fill={C.axis} font-size="10" text-anchor={anchor(k, xlabels.length)}>{l}</text>
	{/each}
	{#each series as s, si (si)}
		{#if bars}
			{@const bw = Math.max(2, g.iw / g.n - 3)}
			{#each s.vals as v, i (i)}
				{#if v != null}
					<rect x={g.sx(i) - bw / 2} y={g.sy(v)} width={bw} height={g.sy(yMin) - g.sy(v)} rx="1.5" fill={s.colorFn ? s.colorFn(v) : s.color} opacity={s.op ?? 0.85} />
				{/if}
			{/each}
		{:else}
			<path d={path(s)} fill="none" stroke={s.color} stroke-width={s.sw ?? 1.6} stroke-linejoin="round" stroke-dasharray={s.dash ? '3 3' : undefined} />
			{#if s.dots}
				{#each s.vals as v, i (i)}
					{#if v != null}
						<circle
							cx={g.sx(i)}
							cy={g.sy(v)}
							r="2.3"
							fill={s.color}
							role="presentation"
							onmousemove={(e) => onpoint?.(i, e)}
							onmouseleave={() => onpoint?.(-1, null)}
						/>
					{/if}
				{/each}
			{/if}
		{/if}
	{/each}
</svg>
