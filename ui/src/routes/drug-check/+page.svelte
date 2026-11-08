<script lang="ts">
	// Mockup screen #drug-check: gene × drug from OwnGenome findings, phenoconversion by current
	// medicines, relevant labs and allergies, with CPIC guidance quoted verbatim. Quotes, never advice.
	import '#lib/components/pages2/pages2.css';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { getDrugCheckView, listRecords } from '#lib/api';
	import { ApiError } from '#lib/api/core';
	import type { DrugCheckView, RecordOut } from '#lib/api/types';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import StatusChip from '#lib/components/StatusChip.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';

	const prof = $derived(currentProfile());
	const drug = $derived(page.url.searchParams.get('drug') ?? '');
	let query = $state('');
	$effect(() => {
		query = drug;
	});
	const check = new Loader<DrugCheckView>();
	const allergies = new Loader<RecordOut[]>();

	$effect(() => {
		const slug = prof?.slug;
		const d = drug.trim();
		void i18n.lang;
		if (!slug || !d) return;
		check.run(() => getDrugCheckView(slug, d, i18n.lang), `${slug}/${d}`);
	});
	$effect(() => {
		const slug = prof?.slug;
		if (!slug) return;
		allergies.run(async () => (await listRecords(slug, 'allergy')).items, slug);
	});

	function submit(e: SubmitEvent) {
		e.preventDefault();
		const q = query.trim();
		if (q) goto(`?drug=${encodeURIComponent(q)}`);
	}
	const c = $derived(drug ? check.data : null);
	const unknown = $derived(check.error instanceof ApiError && check.error.code === 'drug_unknown');
	const relTone = (r: string): 'watch' | 'info' | 'stale' => (r.startsWith('inhibitor') || r.startsWith('inducer') ? 'watch' : r === 'same_drug' ? 'info' : 'stale');
	const EXAMPLES = ['clopidogrel', 'codeine', 'warfarin', 'simvastatin'];
	const labSrc = (l: DrugCheckView['relevant_labs'][number]) => [l.date].filter(Boolean).join(' · ');
	const allergyItems = $derived((allergies.data ?? []).filter((r) => r.status === 'current' || r.status === 'provisional'));
</script>

<div class="ph">
	<div>
		<h1>{t('nav_drug')}</h1>
		<div class="sub">{prof?.display_name ?? ''} · {t('i2_drug_sub')}</div>
	</div>
	<div class="right"><PhaseChip phase="v0.1" /><PhaseChip phase={t('i2_drug_ph')} /></div>
</div>

<form class="search big" role="search" onsubmit={submit}>
	<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#9aa3b2" stroke-width="2" aria-hidden="true"><circle cx="11" cy="11" r="7" /><path d="m20 20-3.5-3.5" /></svg>
	<input bind:value={query} placeholder={t('i2_drug_ph_in')} aria-label={t('i2_drug_ph_in')} autocomplete="off" />
	{#if c}
		{#if c.drug.cpic_drug_id}<span class="chip mono">{c.drug.cpic_drug_id}</span>{/if}
		{#each c.drug.genes as gname (gname)}<span class="chip">{gname}</span>{/each}
	{/if}
</form>

{#if !drug}
	<div class="card">
		<div class="empty">
			{t('i2_drug_empty')}
			<div class="p2row" style="justify-content:center;margin-top:8px">
				{#each EXAMPLES as ex (ex)}<a class="chip" href="?drug={ex}">{ex}</a>{/each}
			</div>
		</div>
	</div>
{:else if unknown}
	<div class="card"><div class="empty">{tf('i2_drug_unknown', { drug })}</div></div>
{:else if check.error}
	<NotReady error={check.error} />
{:else if check.loading && !c}
	<div class="empty">{t('loading')}</div>
{:else if c}
	<div class="drug">
		<div class="grid" style="align-content:start">
			{#each c.pgx as x (x.gene)}
				{@const recs = c.guidance.filter((gd) => gd.applies_to.startsWith(x.gene))}
				<section class="card" style={recs.length ? 'border-color:rgba(224,168,74,.45)' : ''}>
					<h3>
						{t('i2_pgx')}
						{#if recs.length}<span class="pill p-watch"><i aria-hidden="true"></i>{tf('i2_recs', { n: recs.length })}</span>{:else}<span class="pill p-stale"><i aria-hidden="true"></i>{t('i2_no_recs')}</span>{/if}
						{#if x.cpic_level}<span class="r">{t('i2_cpic_level')} {x.cpic_level}</span>{/if}
					</h3>
					<div style="display:flex;gap:22px;align-items:center;margin:4px 0 8px">
						<div><div class="dim" style="font-size:11px">{t('i2_g_gene')}</div><div style="font-size:18px;font-weight:650">{x.gene}</div></div>
						<div><div class="dim" style="font-size:11px">{t('i2_diplotype')}</div><div style="font-size:18px;font-weight:650" class="mono">{x.diplotype ?? '—'}</div></div>
						<div><div class="dim" style="font-size:11px">{t('i2_phenotype')}</div><div style="font-size:15px;font-weight:600;color:{recs.length ? 'var(--watch)' : 'var(--text)'}">{x.phenotype ?? '—'}</div></div>
						<div style="margin-left:auto;text-align:right">
							<span class="pgx ev-{x.evidence}">{x.evidence}</span>
							<span class="pill {x.confidence === 'high' ? 'p-ok' : x.confidence === 'medium' ? 'p-watch' : 'p-stale'}"><i aria-hidden="true"></i>{t('i2_conf_' + x.confidence)}</span>
							<div style="margin-top:4px"><SourceChip label={`OwnGenome · ${x.call_state ?? 'n/a'}`} src={x.src} onopen={openSource} /></div>
						</div>
					</div>
					{#each recs as gd (gd.quote)}
						<div class="dim" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em">{gd.source} · {gd.applies_to}{gd.population ? ' · ' + gd.population : ''}</div>
						<blockquote>“{gd.quote}” <span class="dim">{t('i2_classification')}: {gd.strength.toLowerCase()}</span></blockquote>
					{/each}
					{#if recs.length}
						<div class="dim" style="font-size:11px">{[...new Set(recs.map((r) => r.url))].join(' · ')}</div>
						<div class="note" style="margin-top:8px">{t('i2_indication')}</div>
					{/if}
				</section>
			{:else}
				<section class="card">
					<h3>{t('i2_pgx')}</h3>
					<div class="muted">{c.withheld.by_scope.includes('read:genome') ? t('i2_withheld_genome') : t('i2_no_pgx')}</div>
				</section>
			{/each}

			{#if c.phenoconversion.length}
				<section class="card">
					<h3>{t('i2_phenoconv')}{#if c.phenoconversion[0].rule}<span class="r">rule {c.phenoconversion[0].rule}</span>{/if}</h3>
					{#each c.phenoconversion as pc (pc.gene + pc.by)}
						<div style="display:flex;gap:10px;align-items:center">
							{#if pc.computed}<span class="pill p-info"><i aria-hidden="true"></i>{t('i2_computed')}</span>{:else}<span class="pill p-stale"><i aria-hidden="true"></i>{t('i2_not_computed')}</span>{/if}
							<span>{pc.text}</span>
							<SourceChip label={pc.src} src={pc.src} onopen={openSource} />
						</div>
					{/each}
				</section>
			{/if}

			<section class="card">
				<h3>{t('i2_rel_labs')}<span class="r">{t('i2_latest')}</span></h3>
				{#each c.relevant_labs as l (l.alias_group)}
					<div class="lab">
						<span style="flex:1">{l.name} <span class="dim" style="font-size:11px">· {l.reason}</span></span>
						{#if l.src}<SourceChip label={labSrc(l)} src={l.src} onopen={openSource} />{/if}
						<span class="num" style="font-weight:600;width:130px;text-align:right">{l.value != null ? `${l.value} ${l.unit ?? ''}` : '—'}</span>
						<StatusChip status={l.status} />
					</div>
				{:else}
					<div class="muted">{c.withheld.by_scope.includes('read:labs') ? t('i2_withheld_labs') : t('i2_no_labs')}</div>
				{/each}
			</section>
		</div>

		<div class="grid" style="align-content:start">
			<section class="card">
				<h3>{t('meds')}</h3>
				{#if c.current_medications_relevant.length}
					<table class="t">
						<tbody>
							{#each c.current_medications_relevant as m (m.src + m.name)}
								<tr>
									<td style="white-space:nowrap"><b>{m.name}</b></td>
									<td><StatusChip status={relTone(m.relation)} label={m.relation_text} /></td>
									<td class="muted" style="font-size:12px">{m.since ? `${t('i2_since')} ${m.since} · ` : ''}<SourceChip label={`Rx · ${m.src}`} src={m.src} onopen={openSource} /></td>
								</tr>
							{/each}
						</tbody>
					</table>
				{:else}
					<div class="muted">{c.withheld.by_scope.includes('read:records') ? t('i2_withheld_records') : t('i2_no_meds')}</div>
				{/if}
				{#each c.not_assessed as na (na)}<div class="note" style="margin-top:6px">not_assessed: {na}</div>{/each}
			</section>
			<section class="card">
				<h3>{t('i2_allergies')}</h3>
				{#if allergies.error}
					<NotReady error={allergies.error} />
				{:else if allergyItems.length}
					{#each allergyItems as a (a.id)}<div><b>{a.title}</b> <SourceChip label={a.src} src={a.src} onopen={openSource} /></div>{/each}
				{:else if allergies.data}
					<div class="muted">{t('i2_no_allergy')}</div>
				{/if}
			</section>
			{#if c.limitations.length}
				<section class="card">
					<h3>{t('i2_limits')}<span class="r">CPIC {c.db_versions.cpic ?? ''}</span></h3>
					{#each c.limitations as l (l)}<div class="note" style="margin-bottom:4px">{l}</div>{/each}
				</section>
			{/if}
		</div>
	</div>
{/if}

<div class="disc">
	<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#6ea8fe" stroke-width="2" aria-hidden="true"><circle cx="12" cy="12" r="9" /><path d="M12 8v.5M12 11v5" /></svg>
	<div><b>{t('i2_disc_b')}</b> {t('i2_disc')}</div>
	{#if drug && prof}<span class="mono" style="margin-left:auto;font-size:11px;color:var(--muted);white-space:nowrap">MCP: check_drug(profile="{prof.slug}", drug="{drug}")</span>{/if}
</div>

<SourcePanel />

<style>
	.search.big {
		max-width: none;
		height: 42px;
		margin-bottom: 12px;
		background: var(--raised);
	}
	.search.big input {
		font-size: 15px;
	}
	.lab {
		display: flex;
		gap: 10px;
		align-items: center;
		padding: 5px 0;
		border-top: 1px solid #20252f;
	}
	.lab:first-of-type {
		border-top: 0;
	}
</style>
