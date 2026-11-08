<script lang="ts">
	// Wearables (mockup screen #wearables): daily rollups from GET /profiles/{p}/wearables/summary (lane C),
	// one chart per metric with its average, its coverage and the days it is missing.
	import { getWearables } from '#lib/api';
	import type { WearableMetric, WearablesSummary } from '#lib/api/types';
	import { C } from '#lib/components/colors';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import TrendChart from '#lib/components/TrendChart.svelte';
	import { fmtNum, i18n, t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';

	// How each known metric draws, as in the mockup. Unknown metrics get a plain line.
	const LOOK: Record<string, { bars?: boolean; color: string; scale?: number; unit?: string; sw?: number }> = {
		sleep_asleep: { bars: true, color: '#5b84c9', scale: 1 / 60, unit: 'h' },
		sleep_in_bed: { bars: true, color: '#3f5f99', scale: 1 / 60, unit: 'h' },
		hrv_rmssd: { color: '#8fb4f6' },
		hr_resting: { color: '#e58f96' },
		steps: { bars: true, color: '#3fb68b' },
		vo2max: { color: '#3fb68b', sw: 2 }
	};
	const ORDER = ['sleep_asleep', 'hrv_rmssd', 'hr_resting', 'steps', 'vo2max'];

	const profile = $derived(currentProfile());
	const w = new Loader<WearablesSummary>();
	let days = $state(30);

	function load() {
		const slug = profile?.slug;
		const lang = i18n.lang;
		if (slug) w.run(() => getWearables(slug, lang));
	}
	$effect(load);

	const rank = (m: WearableMetric) => (ORDER.includes(m.metric) ? ORDER.indexOf(m.metric) : ORDER.length);
	const metrics = $derived([...(w.data?.metrics ?? [])].sort((a, b) => rank(a) - rank(b)));
	// The longest series decides which windows make sense; never offer a window the data cannot fill.
	const span = $derived(Math.max(0, ...metrics.map((m) => m.series.length)));
	const windows = $derived([7, 30, 365].filter((d) => d === 7 || span >= d));
	const fmtDay = (d: string) => new Intl.DateTimeFormat(i18n.lang === 'es' ? 'es-ES' : 'en-GB', { day: 'numeric', month: 'short', timeZone: 'UTC' }).format(new Date(d));

	function chart(m: WearableMetric) {
		const look = LOOK[m.metric] ?? { color: C.accent };
		const k = look.scale ?? 1;
		const ser = m.series.slice(-days);
		const vals = ser.map((p) => (p.value == null ? null : +(p.value * k).toFixed(2)));
		const nums = vals.filter((v): v is number => v != null);
		if (!nums.length) return null;
		let lo = Math.min(...nums);
		let hi = Math.max(...nums);
		const step = nice((hi - lo || Math.abs(hi) || 1) / 4);
		lo = look.bars ? 0 : Math.floor((lo - step * 0.4) / step) * step;
		hi = Math.ceil((hi + step * 0.2) / step) * step;
		if (look.bars && m.metric === 'sleep_asleep') lo = Math.max(0, Math.floor(Math.min(...nums)) - 2);
		const ticks: number[] = [];
		for (let v = lo; v <= hi + 1e-9; v += step) ticks.push(+v.toFixed(4));
		const n = ser.length;
		const idx = [...new Set([0, Math.round((n - 1) / 3), Math.round((2 * (n - 1)) / 3), n - 1])];
		const fmt = (v: number) => (m.metric === 'steps' ? `${fmtNum(v / 1000)}k` : look.unit ? `${fmtNum(v)}${look.unit}` : fmtNum(v));
		return {
			series: [{ vals, color: look.color, dots: !look.bars, sw: look.sw }],
			yMin: lo,
			yMax: hi,
			ticks,
			xlabels: idx.map((i) => [i, fmtDay(ser[i].date)] as [number, string]),
			bars: !!look.bars,
			fmt,
			missing: vals.filter((v) => v == null).length,
			unit: look.unit ?? m.unit,
			k
		};
	}
	function nice(raw: number): number {
		const p = 10 ** Math.floor(Math.log10(raw));
		const r = raw / p;
		return (r <= 1 ? 1 : r <= 2 ? 2 : r <= 2.5 ? 2.5 : r <= 5 ? 5 : 10) * p;
	}
	const fmtVal = (m: WearableMetric, v: number | null, k: number, unit: string) => {
		if (v == null) return '—';
		const x = v * k;
		if (unit === 'h') return `${Math.floor(x)} h ${Math.round((x % 1) * 60)} min`;
		const whole = m.metric === 'steps' || unit === 'count' || unit === 'bpm' || unit === 'ms';
		return `${whole ? fmtNum(Math.round(x), 0) : fmtNum(x, 1)}${unit === 'count' ? '' : ' ' + unit}`;
	};
	const ago = (iso: string | null) => (iso ? tf('h1_last_sample', { d: iso.slice(0, 16).replace('T', ' ') }) : '—');
</script>

<div class="ph">
	<div>
		<h1>{t('title_wear')}</h1>
		<div class="sub">{tf('h1_w_sub', { name: profile?.display_name ?? '', d: days })}</div>
	</div>
	<div class="right">
		<PhaseChip phase="Apple Health v0.1" /><PhaseChip phase="Nightscout v0.2" /><PhaseChip phase="Oura, Withings v1.0" />
		{#if windows.length > 1}
			<div class="seg" role="group" aria-label={t('title_wear')}>
				{#each windows as d (d)}
					<button class:on={days === d} aria-pressed={days === d} onclick={() => (days = d)}>{t(d === 7 ? 'h1_w7' : d === 30 ? 'h1_w30' : 'h1_w1y')}</button>
				{/each}
			</div>
		{/if}
	</div>
</div>

{#if w.loading && !w.data}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if w.error}
	<div class="card"><NotReady error={w.error} onretry={load} /></div>
{:else if w.data && !w.data.metrics.length}
	<div class="card"><div class="empty">{t('h1_w_none')}</div></div>
{:else if w.data}
	<div class="devs">
		{#each w.data.devices as d (d.source_id)}
			<div class="dev">
				<div style="flex:1;min-width:0">
					<div style="font-weight:600">{d.device ?? d.provider}</div>
					<div class="dim" style="font-size:11px">{d.provider} · {ago(d.last_sample)}</div>
				</div>
				<div style="width:76px;text-align:right">
					<div class="num" style="font-weight:650">{fmtNum(d.coverage_pct, 0)}%</div>
					<div class="bar"><i style="width:{d.coverage_pct}%;background:{d.coverage_pct > 90 ? C.ok : C.watch}"></i></div>
					<div class="dim" style="font-size:10px">{t('h1_coverage')}</div>
				</div>
			</div>
		{/each}
	</div>
	<div class="wg">
		{#each metrics as m (m.metric)}
			{@const c = chart(m)}
			<div class="card">
				<h3>
					{m.label}
					<span class="r" title={c ? tf('h1_latest', { v: fmtVal(m, m.latest, c.k, c.unit) }) : undefined}>{#if c}{tf('h1_avg', { v: fmtVal(m, m.avg_30d, c.k, c.unit) })}{/if}</span>
				</h3>
				{#if c}
					<TrendChart series={c.series} yMin={c.yMin} yMax={c.yMax} ticks={c.ticks} xlabels={c.xlabels} bars={c.bars} fmt={c.fmt} />
				{:else}
					<div class="empty">{t('h1_nomark')}</div>
				{/if}
				<div style="display:flex;justify-content:space-between;margin-top:2px">
					<span class="src" style="cursor:default"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h8l4 4v14H6z" /><path d="M14 3v4h4" /></svg>{m.source}</span>
					<span class="dim" style="font-size:11px">
						{#if m.metric === 'vo2max'}{t('h1_estimate')}{:else if c?.missing}{tf('h1_missing', { n: c.missing })}{:else}{tf('h1_cov_pct', { n: fmtNum(m.coverage_pct, 0) })}{/if}
					</span>
				</div>
			</div>
		{/each}
		<div class="card">
			<h3>{t('h1_cgm')}<span class="r"><PhaseChip phase="v0.2" /></span></h3>
			<div class="empty">{t('h1_cgm_d')}</div>
		</div>
	</div>
{/if}
