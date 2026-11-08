<script lang="ts">
	// Mockup screen #genome: findings imported from OwnGenome, with evidence levels, call_state, and
	// sensitive findings behind the profile's consent switch. OwnHealth runs no genome engine.
	import '#lib/components/pages2/pages2.css';
	import { dismissReview, getGenomeView, listReview, lookupVariant, setGenomeConsent } from '#lib/api';
	import type { GenomeFindingFull, GenomeView, ReviewItem } from '#lib/api/types';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, tf, tp } from '#lib/components/pages1/util.svelte';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import { day, plain } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';

	const OWNGENOME = 'https://github.com/GeiserX/OwnGenome';

	function tileDetail(tile: GenomeView['tiles'][number]): string {
		if (tile.genes === undefined) return tile.detail;
		const parts = [tile.actionable ? tp('i2_actionable', tile.actionable) : '', tile.genes, tile.hidden ? tp('h1_hidden', tile.hidden) : ''];
		return parts.filter(Boolean).join(' · ');
	}
	const prof = $derived(currentProfile());
	const gen = new Loader<GenomeView>();
	const alerts = new Loader<ReviewItem[]>();
	let actionError = $state<string | null>(null);
	let lookupOpen = $state(false);
	let rsid = $state('');
	const lookup = new Loader<Record<string, unknown>>();

	function load() {
		const slug = prof?.slug;
		if (!slug) return;
		gen.run(() => getGenomeView(slug, i18n.lang));
		alerts.run(async () => (await listReview({ status: 'open', profile: slug, kind: 'genome_reinterpretation' })).items);
	}
	$effect(() => {
		void prof?.slug;
		void i18n.lang;
		load();
	});

	async function consent(ui: boolean) {
		const slug = prof?.slug;
		if (!slug) return;
		if (ui && !confirm(tf('i2_reveal_confirm', { name: prof?.display_name ?? '' }))) return;
		actionError = null;
		try {
			await withStepUp(() => setGenomeConsent(slug, { ui }));
			load();
		} catch (e) {
			actionError = e instanceof Error ? e.message : String(e);
		}
	}

	const g = $derived(gen.data);
	const ds = $derived(g?.dataset ?? null);
	const v = (f: GenomeFindingFull, k: string) => f.value?.[k] as string | number | boolean | string[] | undefined;
	function variantCell(f: GenomeFindingFull): string {
		if (f.category === 'prs') return String(v(f, 'pgs_id') ?? '');
		if (f.category === 'hla') return String(v(f, 'marker') ?? '');
		const ids = f.variants?.length ? f.variants.map((x) => x.rsid).filter(Boolean) : [f.rsid];
		return [ids.join(', '), f.hgvs].filter(Boolean).join(' · ');
	}
	function resultCell(f: GenomeFindingFull): string {
		if (f.diplotype || f.phenotype) return [f.diplotype, f.phenotype].filter(Boolean).join(' · ');
		if (f.category === 'prs') return tf('i2_prs_pct', { p: String(v(f, 'percentile') ?? '—'), pop: String(v(f, 'most_similar_pop') ?? '') });
		if (f.category === 'hla') return `${(v(f, 'alleles') as string[] | undefined)?.join(' · ') ?? ''} · ${v(f, 'marker')} ${v(f, 'present') ? t('i2_present') : t('i2_absent')}`;
		if (f.category === 'trait') return `${v(f, 'trait') ?? ''} ${v(f, 'call') ?? ''}`.trim();
		return [f.zygosity ? t('i2_zyg_' + f.zygosity) : null, f.condition?.name].filter(Boolean).join(' · ');
	}
	function noteCell(f: GenomeFindingFull): string {
		const parts = [f.evidence_native, f.clinvar ? `VariationID ${f.clinvar.variation_id}` : null, f.drugs?.length ? f.drugs.join(', ') : null];
		return parts.filter(Boolean).join(' · ');
	}
	const conf = (c: string) => (c === 'high' ? 'p-ok' : c === 'medium' ? 'p-watch' : c === 'low' ? 'p-stale' : 'p-out');
	const sub = $derived.by(() => {
		if (!ds || !prof) return prof?.display_name ?? '';
		return [prof.display_name, [ds.assay.toUpperCase(), ds.mean_depth ? `${Math.round(ds.mean_depth)}×` : null].filter(Boolean).join(' '), ds.build].join(' · ');
	});
	const subTail = $derived(ds ? [...Object.entries(ds.db_versions).filter(([k]) => k === 'clinvar' || k === 'cpic').map(([k, val]) => `${k === 'clinvar' ? 'ClinVar' : 'CPIC'} ${val}`), `${t('i2_ingested')} ${day(ds.imported_at)}`].join(' · ') : '');
	const sensitiveShown = $derived((g?.findings ?? []).filter((f) => f.sensitive));
	const coverage = $derived.by(() => {
		const c = g?.coverage ?? [];
		return { assessed: c.filter((x) => x.state === 'assessed').length, partial: c.filter((x) => x.state === 'partial').map((x) => x.target), total: c.length };
	});
</script>

<div class="ph">
	<div>
		<h1>{t('nav_gen')}</h1>
		<div class="sub">
			{#if ds}
				{sub} · <span class="mono">genome-facts.json</span> {t('i2_from')} <a href={OWNGENOME} target="_blank" rel="noopener">OwnGenome</a> {ds.source_tool.replace(/^OwnGenome\s*/, 'v')} · {subTail}
			{:else}
				{prof?.display_name ?? ''}
			{/if}
		</div>
	</div>
	<div class="right">
		<PhaseChip phase="v0.1" /><PhaseChip phase={t('i2_gen_ph')} />
		<button class="btn" onclick={() => (lookupOpen = !lookupOpen)} aria-expanded={lookupOpen} disabled={!prof?.scopes.includes('read:genome_raw')} title={prof?.scopes.includes('read:genome_raw') ? '' : 'read:genome_raw'}>{t('i2_lookup')}</button>
	</div>
</div>

{#if lookupOpen && prof}
	<form class="card p2row" style="margin-bottom:12px" onsubmit={(e) => { e.preventDefault(); lookup.run(() => lookupVariant(prof.slug, rsid.trim())); }}>
		<label for="rsid" class="dim">rsID</label>
		<input id="rsid" class="p2in mono" bind:value={rsid} placeholder="rs4244285" required pattern="rs\d+" />
		<button class="btn sm" type="submit">{t('i2_lookup')}</button>
		<span class="dim" style="font-size:11.5px">{t('i2_lookup_d')}</span>
		{#if lookup.error}<span class="p2err">{lookup.error.message}</span>{/if}
		{#if lookup.data}<pre class="code mono" style="flex-basis:100%;max-height:240px;overflow:auto">{JSON.stringify(lookup.data, null, 2)}</pre>{/if}
	</form>
{/if}

{#each alerts.data ?? [] as a (a.id)}
	{@const p = a.payload as Record<string, string>}
	<div class="banner" role="status">
		<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#e0a84a" stroke-width="2" aria-hidden="true"><path d="M12 3 2 20h20zM12 10v4M12 17v.5" /></svg>
		<div><b>{t('i2_reinterp')}</b><div class="muted" style="font-size:12px">{p.summary ?? p.text ?? [p.gene, p.from, '→', p.to].filter(Boolean).join(' ')}</div></div>
		<button class="btn sm" style="margin-left:auto" onclick={() => dismissReview(a.id, 'read').then(load)}>{t('i2_mark_read')}</button>
	</div>
{/each}

{#if actionError}<div class="p2err" role="alert" style="margin-bottom:8px">{actionError}</div>{/if}

{#if gen.error}
	<NotReady error={gen.error} onretry={load} />
{:else if gen.loading && !g}
	<div class="empty">{t('loading')}</div>
{:else if g && !ds}
	<div class="card promo">
		<h3>{t('i2_gen_empty_t')}</h3>
		<p class="muted" style="margin:0 0 10px">{t('i2_gen_empty_d')}</p>
		<div class="p2row">
			<a class="btn pri" href={OWNGENOME} target="_blank" rel="noopener">{t('i2_gen_get')}</a>
			<a class="btn" href="/import-review">{t('i2_gen_import')}</a>
			<span class="dim mono" style="font-size:11.5px">{OWNGENOME}</span>
		</div>
	</div>
{:else if g}
	<div class="tiles">
		{#each g.tiles as tile (tile.key)}
			<div class="tile">
				<div class="k">{t('i2_cat_' + tile.key)}</div>
				<div class="n num">{tile.value}</div>
				<div class="d">{tileDetail(tile)}</div>
			</div>
		{/each}
	</div>
	<div class="card" style="padding:4px 6px">
		<table class="t">
			<thead>
				<tr><th>{t('i2_g_cat')}</th><th>{t('i2_g_gene')}</th><th>{t('i2_g_var')}</th><th>{t('i2_g_res')}</th><th>{t('i2_g_ev')}</th><th>{t('i2_g_conf')}</th><th>call_state</th><th>{t('i2_g_note')}</th></tr>
			</thead>
			<tbody>
				{#each g.findings as f (f.id)}
					<tr>
						<td><span class="chip mono">{f.category}</span></td>
						<td><b>{f.gene ?? '—'}</b></td>
						<td class="mono muted">{variantCell(f)}</td>
						<td>
							<button class="lnk" onclick={() => openSource(f.src)} title={t('open_src')}>{resultCell(f)}</button>
							{#if f.confirm_clinically}<div class="dim" style="font-size:11px">{t('i2_confirm_clin')}</div>{/if}
						</td>
						<td><span class="pgx ev-{f.evidence}" title={t('i2_ev_' + f.evidence)}>{f.evidence}</span></td>
						<td><span class="pill {conf(f.confidence)}"><i aria-hidden="true"></i>{t('i2_conf_' + f.confidence)}</span></td>
						<td class="mono" style="color:{f.call_state === 'alt' ? 'var(--text)' : 'var(--muted)'}">{f.call_state ?? 'n/a'}</td>
						<td class="muted" style="font-size:12px">
							{noteCell(f) || '—'}
							{#if f.sensitive}<div>{t('i2_sens_shown')} · <button class="lnk" onclick={() => consent(false)}>{t('i2_hide')}</button></div>{/if}
						</td>
					</tr>
				{/each}
				{#each g.locked as l, i (i)}
					<tr class="lock">
						<td><span class="chip mono">{l.category}</span></td>
						<td colspan="6"><span style="color:var(--muted)"><span aria-hidden="true">🔒 </span>{tf('i2_locked', { gene: l.gene ?? '—' })} <span class="mono">read:sensitive</span>.</span></td>
						<td>
							{#if g.consent.can_change}<button class="btn sm" onclick={() => consent(true)}>{t('i2_reveal')}</button>{:else}<span class="dim" style="font-size:11px">{t('i2_owner_only')}</span>{/if}
						</td>
					</tr>
				{/each}
				{#if g.withheld.minor_rule}
					<tr class="lock"><td colspan="8"><span class="muted"><span aria-hidden="true">🔒 </span>{tf('i2_minor_rule', { n: g.withheld.minor_rule })}</span></td></tr>
				{/if}
			</tbody>
		</table>
	</div>
	{#if sensitiveShown.length}<div class="sr-only" role="status">{t('i2_sens_shown')}</div>{/if}
{/if}

<div class="note" style="margin-top:10px">
	<span>{t('i2_gen_from1')} <a href={OWNGENOME} target="_blank" rel="noopener">OwnGenome</a> ({OWNGENOME}) {t('i2_gen_from2')}</span>
</div>
{#if g && ds}
	<div style="display:flex;gap:18px;margin-top:6px" class="note cov">
		<span>
			<b style="color:var(--text)">{t('i2_coverage')}</b> · {tf('i2_cov_line', { a: coverage.assessed, n: coverage.total })}{#if coverage.partial.length} · {t('i2_cov_partial')}: {coverage.partial.join(', ')}{/if}
			· call_state: ref / alt / no_call / not_on_array / not_covered; n/a {t('i2_cs_na')}
			{#if g.not_assessed.length} · not_assessed: {g.not_assessed.map((x) => x.gene ?? x.panel).join(', ')}{/if}
			{#each g.limitations as l (l)} · {l}{/each}
		</span>
		<span style="margin-left:auto">{t('i2_ev_legend')}</span>
	</div>
	<div class="note" style="margin-top:6px">{plain(g.notice)}</div>
{/if}

<SourcePanel />
<StepUpDialog />

<style>
	.lnk {
		background: none;
		border: 0;
		padding: 0;
		color: inherit;
		text-align: left;
		font: inherit;
	}
	button.lnk:hover {
		color: var(--accent);
	}
	.promo {
		border-color: rgba(110, 168, 254, 0.35);
	}
	.cov > span:first-child {
		flex: 1;
	}
	.cov > span:last-child {
		flex: none;
		max-width: 40%;
	}
</style>
