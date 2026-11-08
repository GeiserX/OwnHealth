<script lang="ts">
	// Biomarkers (mockup screen #biomarker). Without ?code= it lists the profile's analytes; with it, the
	// detail: trend over the lab's printed reference band, a source chip per point, the printed string on
	// hover and a unit toggle. Values come from lane A (observations, trend); status words from lane E.
	import { page } from '$app/state';
	import { getDashboard, getTrend, searchObservations, type TrendPointStatus, type Units } from '#lib/api';
	import type { Dashboard, MarkerRow, ObservationOut, Trend } from '#lib/api/types';
	import { C } from '#lib/components/colors';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import Tip from '#lib/components/pages1/Tip.svelte';
	import { Loader, tf, tp, yfrac } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import ManualEntry from '#lib/components/ManualEntry.svelte';
	import Sparkline from '#lib/components/Sparkline.svelte';
	import StatusChip from '#lib/components/StatusChip.svelte';
	import { fmtNum, i18n, t } from '#lib/i18n/index.svelte';
	import { buildBars, niceStep } from '#lib/logic/axis';
	import { currentProfile } from '#lib/state/app.svelte';

	const profile = $derived(currentProfile());
	const code = $derived(page.url.searchParams.get('code'));
	// ?q= from the top-bar search filters the list.
	const query = $derived(page.url.searchParams.get('q') ?? '');
	// 'direct' / 'calculated' and other method words, translated when the dict knows them.
	const methodLabel = (m: string) => {
		const k = 'h1_method_' + m.toLowerCase();
		return t(k) === k ? m : t(k);
	};

	// ---- list ----
	const obs = new Loader<{ items: ObservationOut[] }>();
	// ---- detail: one trend per unit system, so the toggle shows only units that really differ ----
	const trends = new Loader<Trend[]>();
	const dash = new Loader<Dashboard>();
	let unitIdx = $state(0);
	let adding = $state(false);

	function load() {
		const slug = profile?.slug;
		const lang = i18n.lang;
		if (!slug) return;
		if (!code) {
			const q = query || undefined;
			obs.run(() => searchObservations(slug, { category: 'laboratory', lang, q }));
			return;
		}
		unitIdx = 0;
		const c = code;
		trends.run(async () => {
			const all = await Promise.allSettled((['as_reported', 'si', 'conventional'] as Units[]).map((u) => getTrend(slug, c, 'all', u, lang)));
			const ok = all.flatMap((r) => (r.status === 'fulfilled' ? [r.value] : []));
			if (!ok.length) throw (all[0] as PromiseRejectedResult).reason;
			return ok.filter((tr, i) => ok.findIndex((o) => o.unit === tr.unit) === i);
		});
		dash.run(() => getDashboard(slug, lang));
	}
	$effect(load);

	// List rows: latest value per alias group, newest first.
	const groups = $derived.by(() => {
		const by = new Map<string, { latest: ObservationOut; n: number }>();
		for (const o of obs.data?.items ?? []) {
			const g = by.get(o.alias_group);
			if (!g) by.set(o.alias_group, { latest: o, n: 1 });
			else {
				g.n++;
				if (o.date > g.latest.date) g.latest = o;
			}
		}
		return [...by.values()].sort((a, b) => a.latest.name.localeCompare(b.latest.name));
	});

	const tr = $derived(trends.data?.[Math.min(unitIdx, (trends.data?.length ?? 1) - 1)] ?? null);
	const pts = $derived((tr?.points ?? []) as TrendPointStatus[]);
	const last = $derived(pts.length ? pts[pts.length - 1] : null);
	const labs = $derived([...new Set(pts.map((p) => p.lab ?? '—'))]);
	// The dashboard card holding this marker gives its status and the related markers.
	const card = $derived(dash.data?.systems.find((s) => s.markers.some((m) => m.alias_group === tr?.alias_group)) ?? null);
	const marker = $derived(card?.markers.find((m) => m.alias_group === tr?.alias_group) ?? null);
	const related = $derived((card?.markers ?? []).filter((m) => m.alias_group !== tr?.alias_group) as MarkerRow[]);

	// A point's colour: its status from lane E when present, else the lab's own flag (abnormal or not).
	const ABN = new Set(['L', 'H', 'LL', 'HH', 'A']);
	function ptColor(p: TrendPointStatus): string {
		if (p.status) return C[p.status];
		if (p.flag && ABN.has(p.flag)) return C.out;
		return p.flag === 'N' ? C.ok : C.accent;
	}
	const SHAPES = ['circle', 'square', 'diamond', 'triangle'] as const;
	const GLYPH = { circle: '●', square: '■', diamond: '◆', triangle: '▲' };
	const shapeOf = (lab: string | null) => SHAPES[Math.max(0, labs.indexOf(lab ?? '—')) % SHAPES.length];

	// ---- chart geometry (port of the mockup's ldlChart) ----
	const W = 860;
	const H = 330;
	const pl = 60;
	const pr = 20;
	const pt = 14;
	const pb = 74;
	const iw = W - pl - pr;
	const ih = H - pt - pb;
	const geo = $derived.by(() => {
		if (!pts.length || !tr) return null;
		const asOf = new Date().toISOString().slice(0, 10);
		const x0 = Math.floor(yfrac(pts[0].date));
		const x1 = Math.max(Math.ceil(yfrac(pts[pts.length - 1].date) + 0.01), Math.ceil(yfrac(asOf)));
		const vals = pts.flatMap((p) => [p.value, p.ref_low, p.ref_high]).filter((v): v is number => v != null);
		const lo = Math.min(...vals);
		const hi = Math.max(...vals);
		const step = niceStep((hi - lo || Math.abs(hi) || 1) / 4);
		const y0 = Math.max(0, Math.floor((lo - step * 0.5) / step) * step);
		const y1 = Math.ceil((hi + step * 0.3) / step) * step;
		const sx = (y: number) => pl + ((y - x0) / (x1 - x0)) * iw;
		const sy = (v: number) => pt + ih - ((v - y0) / (y1 - y0 || 1)) * ih;
		const ticks: number[] = [];
		for (let v = y0; v <= y1 + 1e-9; v += step) ticks.push(+v.toFixed(6));
		const years: number[] = [];
		for (let y = x0; y <= x1; y++) years.push(y);
		const xy = pts.map((p) => [sx(yfrac(p.date)), sy(p.value)] as [number, number]);
		// Reference band as printed, one step per point: from halfway to the previous point to halfway to the next.
		const band = pts.map((p, i) => {
			const a = i === 0 ? sx(x0) : (xy[i - 1][0] + xy[i][0]) / 2;
			const b = i === pts.length - 1 ? sx(x1) : (xy[i][0] + xy[i + 1][0]) / 2;
			if (p.ref_low == null && p.ref_high == null) return null;
			const top = sy(Math.min(p.ref_high ?? y1, y1));
			const bot = sy(Math.max(p.ref_low ?? y0, y0));
			return { x: a, w: b - a, y: top, h: Math.max(0, bot - top) };
		});
		// Dashed markers where the lab or the code changes between two points.
		const marks: { x: number; label: string; row: number }[] = [];
		for (let i = 1; i < pts.length; i++) {
			const a = pts[i - 1];
			const b = pts[i];
			const parts = [];
			if ((a.lab ?? '') !== (b.lab ?? '')) parts.push(t('h1_lab_change'));
			if ((a.loinc ?? '') !== (b.loinc ?? '') || (a.method ?? '') !== (b.method ?? '')) parts.push(b.method ? methodLabel(b.method) : t('h1_code_change'));
			if (parts.length) marks.push({ x: (xy[i - 1][0] + xy[i][0]) / 2, label: parts.join(' · '), row: 0 });
		}
		// Each label takes the first row where it does not print over an earlier one (about 5.6 px per character).
		const rowEnd: number[] = [];
		for (const m of marks) {
			let r = rowEnd.findIndex((end) => m.x + 4 > end + 6);
			if (r < 0) r = rowEnd.length;
			rowEnd[r] = m.x + 4 + m.label.length * 5.6;
			m.row = r;
		}
		const clampX = (d: string) => Math.min(sx(x1), Math.max(sx(x0), sx(yfrac(d))));
		// Annotations with an end draw as a bar on the Meds row; one-day ones as a dot on the Events row.
		// A course with no end is still running and draws to today.
		const bars = buildBars(tr.annotations, clampX, asOf);
		return { x0, x1, y0, y1, sx, sy, ticks, years, xy, band, marks, bars };
	});

	// Extra Meds rows when courses overlap: each adds 14 px below the chart.
	const extra = $derived(geo ? Math.max(0, ...geo.bars.filter((b) => b.span).map((b) => b.row)) * 14 : 0);
	// Hover: the printed string and its provenance.
	let tip = $state<{ i: number; x: number; y: number } | null>(null);
	const fmt = (v: number) => fmtNum(v);
	const pctFmt = (v: number) => `${v > 0 ? '+' : v < 0 ? '−' : ''}${fmtNum(Math.abs(v), 1)}%`;
	const change = $derived(pts.length > 1 ? pts[pts.length - 1].value - pts[pts.length - 2].value : null);
	const rangeOf = (p: TrendPointStatus) => p.range ?? (p.ref_low != null || p.ref_high != null ? `${p.ref_low ?? ''}-${p.ref_high ?? ''}` : '—');
</script>

{#if !code}
	<div class="ph">
		<div>
			<h1>{t('nav_bio')}</h1>
			{#if obs.data}<div class="sub">{tp('h1_bio_sub', groups.length, { name: profile?.display_name ?? '' })}</div>{/if}
		</div>
		<div class="right">
			{#if profile && profile.role !== 'viewer'}<button class="btn" onclick={() => (adding = !adding)} aria-expanded={adding}>{t('me_add_value')}</button>{/if}
			<PhaseChip phase="v0.1" />
		</div>
	</div>
	{#if adding && profile}
		<div class="card" style="margin-bottom:12px">
			<h3>{t('me_add_value')}</h3>
			<ManualEntry profile={profile.slug} kinds={['observation']} onsaved={load} oncancel={() => (adding = false)} />
		</div>
	{/if}
	{#if query}<div class="note" style="margin-bottom:6px">{t('me_filtered')} <b>{query}</b> · <a href="/biomarkers">{t('h1_clear')}</a></div>{/if}
	<div class="card" style="padding:6px 6px 2px">
		{#if obs.loading && !obs.data}
			<div class="empty">{t('loading')}</div>
		{:else if obs.error}
			<NotReady error={obs.error} onretry={load} />
		{:else if !groups.length}
			<div class="empty">{t('h1_bio_none')}</div>
		{:else}
			<table class="t nw">
				<thead><tr><th>{t('h1_th_name')}</th><th>{t('h1_th_latest')}</th><th>{t('h1_th_printed')}</th><th>{t('h1_th_date')}</th><th>{t('h1_th_lab')}</th><th style="text-align:right">{t('h1_th_n')}</th><th>{t('h1_th_source')}</th></tr></thead>
				<tbody>
					{#each groups as g (g.latest.alias_group)}
						<tr>
							<td><a href="/biomarkers?code={encodeURIComponent(g.latest.alias_group)}">{g.latest.name}</a></td>
							<td class="num" style="font-weight:600">{g.latest.printed.value} <span class="muted" style="font-weight:400">{g.latest.printed.unit ?? ''}</span></td>
							<td class="mono muted">{g.latest.printed.name}</td>
							<td class="num">{g.latest.date}</td>
							<td>{g.latest.lab ?? '—'}</td>
							<td class="num" style="text-align:right">{g.n}</td>
							<td><SourceChip label={g.latest.src_label ?? g.latest.src} src={g.latest.src} onopen={openSource} /></td>
						</tr>
					{/each}
				</tbody>
			</table>
		{/if}
	</div>
{:else}
	<div class="ph">
		<div>
			<h1>
				{tr?.name ?? code}
				{#each tr?.codes ?? [] as c (c)}
					<span class="chip mono" style="vertical-align:3px">{c.startsWith('LOINC:') ? 'LOINC ' + c.slice(6) : c}</span>{' '}
				{/each}
			</h1>
			<div class="sub">
				{#if pts.length}
					{profile?.display_name} · {tp('h1_meas', pts.length, { labs: tp('h1_labs', labs.length), from: pts[0].date.slice(0, 4), to: pts[pts.length - 1].date.slice(0, 4) })}
				{:else}
					<a href="/biomarkers">← {t('h1_back')}</a>
				{/if}
			</div>
		</div>
		<div class="right"><a class="btn sm" href="/biomarkers">{t('h1_back')}</a><PhaseChip phase="v0.1" /><PhaseChip phase={t('h1_ph_calc')} /></div>
	</div>

	{#if trends.loading && !trends.data}
		<div class="card"><div class="empty">{t('loading')}</div></div>
	{:else if trends.error}
		<div class="card"><NotReady error={trends.error} onretry={load} /></div>
	{:else if tr && !pts.length}
		<div class="card"><div class="empty">{t('h1_trend_none')}</div></div>
	{:else if tr && geo && last}
		<div class="bm">
			<div>
				<div class="card" style="padding:12px 14px 6px">
					<div style="display:flex;align-items:center;gap:16px;margin-bottom:4px">
						<div class="kv"><span class="big num">{fmt(last.value)}</span><span class="muted">{last.unit}</span></div>
						{#if marker}<StatusChip status={marker.status} />{:else if last.status}<StatusChip status={last.status} />{/if}
						<span class="muted" style="font-size:12px">{last.date} · <SourceChip label={last.src_label ?? last.src} src={last.src} onopen={openSource} /></span>
						{#if (trends.data?.length ?? 0) > 1}
							<div class="seg" role="group" aria-label={t('h1_th_value')} style="margin-left:auto">
								{#each trends.data ?? [] as u, i (u.unit)}
									<button class:on={i === unitIdx} aria-pressed={i === unitIdx} onclick={() => (unitIdx = i)}>{u.unit}</button>
								{/each}
							</div>
						{/if}
					</div>
					<div class="legend" style="margin:2px 0 2px">
						<span><i style="width:9px;height:9px;border-radius:50%;background:{C.ok}"></i>{t('h1_lg_ok')}</span>
						<span><i style="width:9px;height:9px;border-radius:50%;background:{C.watch}"></i>{t('h1_lg_watch')}</span>
						<span><i style="width:9px;height:9px;border-radius:50%;background:{C.out}"></i>{t('h1_lg_out')}</span>
						<span class="dim">{t('h1_lg_shape')} {#each labs as l (l)}{GLYPH[shapeOf(l)]} {l}{' '}{/each}</span>
						<span class="dim">{t('h1_lg_flags')}</span>
					</div>
					<svg width="100%" viewBox="0 0 {W} {H + extra}" role="img" aria-label="{tr.name} · {pts.length}">
						{#each geo.ticks as v (v)}
							<line x1={pl} x2={W - pr} y1={geo.sy(v)} y2={geo.sy(v)} stroke={C.grid} />
							<text x={pl - 7} y={geo.sy(v) + 4} fill={C.axis} font-size="11" text-anchor="end">{fmt(v)}</text>
						{/each}
						{#each geo.band as b, i (i)}
							{#if b}<rect x={b.x} y={b.y} width={b.w} height={b.h} fill="#3fb68b" opacity=".08" />{/if}
						{/each}
						<text x={pl + 6} y={geo.sy(geo.y0) - 6} fill="#3fb68b" font-size="10.5" opacity=".9">{t('h1_band')}</text>
						{#each geo.years as y (y)}
							<line x1={geo.sx(y)} x2={geo.sx(y)} y1={pt} y2={pt + ih} stroke={C.grid} opacity=".6" />
							<text x={geo.sx(y)} y={pt + ih + 15} fill={C.axis} font-size="11" text-anchor="middle">{y}</text>
						{/each}
						{#each geo.marks as m, i (i)}
							<line x1={m.x} x2={m.x} y1={pt} y2={pt + ih} stroke="#56607a" stroke-dasharray="2 3" />
							<text x={m.x + 4} y={pt + 10 + m.row * 12} fill="#8a93a5" font-size="10">{m.label}</text>
						{/each}
						<polyline points={geo.xy.map((p) => p.join(',')).join(' ')} fill="none" stroke="#8792a6" stroke-width="1.6" />
						{#each pts as p, i (p.src + "|" + i)}
							{@const [x, y] = geo.xy[i]}
							{@const shape = shapeOf(p.lab)}
							<g
								fill={ptColor(p)}
								stroke="#0f1115"
								stroke-width="1.5"
								style="cursor:pointer"
								role="button"
								tabindex="0"
								aria-label="{p.date} · {p.printed} · {p.lab ?? ''}"
								onmousemove={(e) => (tip = { i, x: e.clientX, y: e.clientY })}
								onmouseleave={() => (tip = null)}
								onfocus={(e) => {
									const r = (e.currentTarget as SVGGElement).getBoundingClientRect();
									tip = { i, x: r.right, y: r.top };
								}}
								onblur={() => (tip = null)}
								onclick={() => openSource(p.src)}
								onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && openSource(p.src)}
							>
								{#if shape === 'circle'}<circle cx={x} cy={y} r="5.5" />
								{:else if shape === 'square'}<rect x={x - 5} y={y - 5} width="10" height="10" rx="1.5" />
								{:else if shape === 'diamond'}<path d="M{x} {y - 7}L{x + 7} {y}L{x} {y + 7}L{x - 7} {y}Z" />
								{:else}<path d="M{x} {y - 6.5}L{x + 6.5} {y + 5}L{x - 6.5} {y + 5}Z" />{/if}
							</g>
							{#if i >= pts.length - 2}
								<text {x} y={y - 11} fill="#e6e8ee" font-size="11" font-weight="600" text-anchor="middle">{fmt(p.value)}</text>
							{/if}
						{/each}
						{#if geo.bars.length}
							{@const my = pt + ih + 30}
							<text x={pl - 7} y={my + 9} fill={C.axis} font-size="10.5" text-anchor="end">{t('h1_meds_row')}</text>
							{#each geo.bars.filter((b) => b.span) as b, i (i)}
								<rect x={b.x} y={my + b.row * 14} width={Math.max(4, b.x2 - b.x)} height="12" rx="3" fill={C.accent} opacity=".55"><title>{b.label}{b.ongoing ? ' · ' + t('h1_ongoing') : ''}</title></rect>
								{#if b.ongoing}<path d="M{b.x2 - 1} {my + b.row * 14 + 2}l5 4-5 4z" fill={C.accent} opacity=".8" />{/if}
								<text x={b.x + 6} y={my + b.row * 14 + 9.5} fill="#0c1222" font-size="10" font-weight="600">{b.x2 - b.x > 110 ? b.label : ''}</text>
							{/each}
							<text x={pl - 7} y={my + 37 + extra} fill={C.axis} font-size="10.5" text-anchor="end">{t('h1_ev_row')}</text>
							{#each geo.bars.filter((b) => !b.span) as b, i (i)}
								<circle cx={b.x} cy={my + 33 + extra} r="4" fill="#c39bd3"><title>{b.label}</title></circle>
								<!-- left of the dot, unless that would print over the row's own label -->
								{@const left = b.x - pl > b.label.length * 5.6 + 14}
								<text x={left ? b.x - 7 : b.x + 7} y={my + 37 + extra} fill="#8a93a5" font-size="10" text-anchor={left ? 'end' : 'start'}>{b.label}</text>
							{/each}
						{/if}
					</svg>
				</div>

				<div class="card" style="margin-top:12px;padding:6px 6px 2px;overflow-x:auto">
					<table class="t nw">
						<thead>
							<tr><th>{t('h1_th_date')}</th><th>{t('h1_th_value')}</th><th>{t('h1_th_printed')}</th><th>{t('h1_th_range')}</th><th>{t('h1_th_loinc')}</th><th>{t('h1_th_lab')}</th><th>{t('h1_th_status')}</th><th>{t('h1_th_source')}</th></tr>
						</thead>
						<tbody>
							{#each pts.slice().reverse() as p, i (p.src + "|" + i)}
								<tr>
									<td class="num">{p.date}</td>
									<td class="num" style="font-weight:600;color:{p.status && p.status !== 'ok' ? C[p.status] : 'inherit'}">{fmt(p.value)} <span class="muted" style="font-weight:400">{p.unit}</span></td>
									<td class="mono muted">{p.printed}</td>
									<td class="num">{rangeOf(p)}</td>
									<td class="mono">{p.loinc ?? '—'}</td>
									<td>{p.lab ?? '—'}</td>
									<td>
										{#if p.status}<StatusChip status={p.status} />
										{:else if p.flag}<span class="flag" title={t('h1_lg_flags')}>{p.flag}</span>
										{:else}<span class="dim">—</span>{/if}
									</td>
									<td><SourceChip label={p.src_label ?? p.src} src={p.src} onopen={openSource} /></td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</div>

			<div class="grid" style="align-content:start">
				<div class="card">
					<h3>{t('h1_rcv_t')}</h3>
					{#if change !== null}
						<div style="display:flex;gap:16px;align-items:baseline">
							<div>
								<div class="dim" style="font-size:11px">{t('h1_change')}</div>
								<div style="font-size:18px;font-weight:650">{change > 0 ? '+' : change < 0 ? '−' : ''}{fmt(Math.abs(+change.toFixed(4)))} {last.unit}</div>
								{#if tr.stats.last_vs_previous_pct != null}<div class="muted" style="font-size:11.5px">{pctFmt(tr.stats.last_vs_previous_pct)}</div>{/if}
							</div>
							{#if tr.stats.rcv_pct != null}
								<div>
									<div class="dim" style="font-size:11px">{t('h1_rcv')}</div>
									<div style="font-size:18px;font-weight:650">±{fmtNum(tr.stats.rcv_pct, 1)}%</div>
								</div>
							{/if}
						</div>
					{/if}
					<div class="note" style="margin-top:6px">
						{tr.stats.exceeds_rcv === true ? t('h1_rcv_yes') : tr.stats.exceeds_rcv === false ? t('h1_rcv_no') : t('h1_rcv_na')}
					</div>
				</div>
				<div class="card">
					<h3>{t('h1_calc')} <span class="r">{t('nodx')}</span></h3>
					<div class="empty" style="padding:8px 0"><PhaseChip phase="v0.3" /> {t('h1_calc_d')}</div>
				</div>
				{#if related.length}
					<div class="card">
						<h3>{t('h1_related')}</h3>
						{#each related as m (m.alias_group)}
							<div style="display:flex;align-items:center;gap:8px;padding:4px 0;border-top:1px solid #20252f">
								<a style="flex:1;color:inherit" href="/biomarkers?code={encodeURIComponent(m.alias_group)}">{m.name}</a>
								<span class="num" style="color:{m.status === 'ok' ? 'inherit' : C[m.status]};font-weight:600">{m.value} {m.unit}<span class="sr-only"> · {t(m.status)}</span></span>
								{#if m.spark.length}<Sparkline values={m.spark} status={m.status} w={60} h={20} />{/if}
							</div>
						{/each}
					</div>
				{/if}
				{#if tr.limitations.length}
					<div class="card">
						<h3>{t('h1_limits')}</h3>
						{#each tr.limitations as l (l)}<div class="note" style="margin-bottom:4px">{l}</div>{/each}
					</div>
				{/if}
			</div>
		</div>
	{/if}
{/if}

{#if tip && pts[tip.i]}
	{@const p = pts[tip.i]}
	<Tip x={tip.x} y={tip.y}>
		<b>{fmt(p.value)} {p.unit}</b>{#if p.status}&nbsp;·&nbsp;<StatusChip status={p.status} />{/if}<br />
		<span class="mono">{p.printed}</span><br />
		{p.date} · {p.lab ?? '—'}{p.method ? ' · ' + methodLabel(p.method) : ''}<br />
		<span class="dim">{t('h1_printed_range')} {rangeOf(p)}{p.loinc ? ' · LOINC ' + p.loinc : ''}</span><br />
		<span style="color:#6ea8fe">{tf('h1_open_page', { p: p.src_label ?? p.src })}</span>
	</Tip>
{/if}

<SourcePanel />
