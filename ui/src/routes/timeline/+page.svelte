<script lang="ts">
	// Timeline (mockup screen #timeline): dated events from GET /profiles/{p}/timeline (lane H), newest first,
	// grouped by year, filterable by type and country, each with its source chip.
	import { getTimeline } from '#lib/api';
	import type { TimelineEvent } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import RecordsCard from '#lib/components/RecordsCard.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';
	import { sourceLabel } from '#lib/logic/format';

	type Ty = TimelineEvent['type'];
	// Colours and glyphs from the mockup's TYPES table.
	const TYPES: [Ty, string, string][] = [
		['visit', '#6ea8fe', '<path d="M12 4v16M4 12h16"/>'],
		['lab', '#3fb68b', '<path d="M9 3v7l-4 9h14l-4-9V3"/>'],
		['imaging', '#c39bd3', '<circle cx="12" cy="12" r="6"/>'],
		['diagnosis', '#ef6b73', '<path d="M12 6v7M12 17v1"/>'],
		['medication', '#e0a84a', '<path d="M6 12h12"/>'],
		['vaccine', '#7fd1d9', '<path d="m6 18 12-12"/>'],
		['genome', '#b7c38c', '<path d="M8 4c0 8 8 8 8 16M16 4c0 8-8 8-8 16"/>']
	];
	const TY = Object.fromEntries(TYPES.map(([k, c, g]) => [k, { c, g }])) as Record<Ty, { c: string; g: string }>;

	const profile = $derived(currentProfile());
	const tl = new Loader<{ events: TimelineEvent[]; withheld: { by_scope: string[] } }>();
	let types = $state(new Set<Ty>());
	let countries = $state(new Set<string>());

	function load() {
		const slug = profile?.slug;
		const lang = i18n.lang;
		if (slug) tl.run(() => getTimeline(slug, { lang }), slug);
	}
	$effect(load);

	const events = $derived(tl.data?.events ?? []);
	const countryList = $derived([...new Set(events.map((e) => e.country).filter((c): c is string => !!c))].sort());
	const shown = $derived(events.filter((e) => (!types.size || types.has(e.type)) && (!countries.size || (e.country && countries.has(e.country)))));
	const count = (k: Ty) => events.filter((e) => e.type === k).length;
	// Sources card: one row per provider (or source label when no provider is recorded).
	const sources = $derived.by(() => {
		const m = new Map<string, { country: string | null; n: number }>();
		for (const e of events) {
			const key = e.provider ?? (e.src_label ? sourceLabel(e.src_label.split(' · ')[0], t('src_manual')) : t('h1_unknown_src'));
			const r = m.get(key) ?? { country: e.country, n: 0 };
			r.n++;
			m.set(key, r);
		}
		return [...m.entries()].sort((a, b) => b[1].n - a[1].n);
	});
	const years = $derived.by(() => {
		const out: { year: string; items: TimelineEvent[] }[] = [];
		for (const e of shown) {
			const y = e.date.slice(0, 4);
			if (out.at(-1)?.year !== y) out.push({ year: y, items: [] });
			out.at(-1)!.items.push(e);
		}
		return out;
	});
	function toggle<T>(set: Set<T>, k: T): Set<T> {
		const s = new Set(set);
		if (s.has(k)) s.delete(k);
		else s.add(k);
		return s;
	}
</script>

<div class="ph">
	<div>
		<h1>{t('title_tl')}</h1>
		{#if tl.data}<div class="sub">{tf('h1_tl_sub', { name: profile?.display_name ?? '', n: events.length, s: sources.length })}</div>{/if}
	</div>
	<div class="right">
		<PhaseChip phase="v0.3" />
		<button class="btn" aria-disabled="true" title={t('h1_soon_btn')}>{t('h1_export')}</button>
	</div>
</div>

{#if tl.loading && !tl.data}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if tl.error}
	<div class="card"><NotReady error={tl.error} onretry={load} /></div>
{:else if tl.data}
	<div class="filters">
		<span class="lab">{t('h1_type')}</span>
		{#each TYPES as [k, c] (k)}
			<button class="chip" class:on={types.has(k)} aria-pressed={types.has(k)} onclick={() => (types = toggle(types, k))}>
				<i style="width:8px;height:8px;border-radius:50%;background:{c};display:inline-block"></i>{t('h1_ty_' + k)} <span class="dim">{count(k)}</span>
			</button>
		{/each}
		{#if countryList.length}
			<span class="lab" style="margin-left:14px">{t('h1_country')}</span>
			{#each countryList as c (c)}
				<button class="chip" class:on={countries.has(c)} aria-pressed={countries.has(c)} onclick={() => (countries = toggle(countries, c))}>{c}</button>
			{/each}
		{/if}
		{#if types.size || countries.size}
			<button class="chip" onclick={() => ((types = new Set()), (countries = new Set()))}>{t('h1_clear')}</button>
		{/if}
	</div>
	<div class="tl">
		<div>
			{#each years as y (y.year)}
				<div class="yr">{y.year}</div>
				{#each y.items as e, i (e.type + e.date + e.title + i)}
					<div class="ev">
						<div class="dt num">{e.date.slice(5)}</div>
						<div class="rail">
							<div class="dot" style="background:{TY[e.type]?.c ?? '#9aa3b2'}"><svg viewBox="0 0 24 24" aria-hidden="true">{@html TY[e.type]?.g ?? ''}</svg></div>
						</div>
						<div class="body">
							<div><div class="t1">{e.title}</div>{#if e.detail}<div class="t2">{e.detail}</div>{/if}</div>
							<div class="rt">
								<span class="chip">{t('h1_ty_' + e.type)}</span>
								{#each e.flags as f (f)}<span class="flag">{f}</span>{/each}
								{#if e.country}<span class="flag">{e.country}</span>{/if}
								{#if e.src}<SourceChip label={e.src_label ?? e.src} src={e.src} onopen={openSource} />{/if}
							</div>
						</div>
					</div>
				{/each}
			{:else}
				<div class="muted" style="padding:30px">{events.length ? t('h1_nomatch') : t('h1_tl_none')}</div>
			{/each}
			{#if tl.data.withheld.by_scope.length}
				<div class="note" style="margin:10px 0 0 74px">{tf('h1_withheld', { list: tl.data.withheld.by_scope.map((k) => t('h1_ty_' + k)).join(', ') })}</div>
			{/if}
		</div>
		<div class="grid" style="align-content:start">
			<div class="card">
				<h3>{t('h1_sources')}</h3>
				{#if sources.length}
					<table class="t">
						<tbody>
							{#each sources as [name, s] (name)}
								<tr><td>{name}</td><td>{#if s.country}<span class="flag">{s.country}</span>{/if}</td><td class="num" style="text-align:right">{s.n}</td></tr>
							{/each}
						</tbody>
					</table>
				{:else}
					<div class="dim" style="font-size:12px">{t('h1_tl_none')}</div>
				{/if}
			</div>
			{#if profile}
				{#key profile.slug}<RecordsCard profile={profile.slug} canWrite={profile.role !== 'viewer'} onchange={load} />{/key}
			{/if}
			<div class="card">
				<h3>{t('h1_gaps')} <span class="r"><PhaseChip phase="v0.3" /></span></h3>
				<div class="dim" style="font-size:12px">{t('h1_gaps_d')}</div>
			</div>
		</div>
	</div>
{/if}

<SourcePanel />
