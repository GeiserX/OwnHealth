<script lang="ts">
	// The patterns card of the Family screen (v0.3 family history): the patterns a guideline names that
	// this profile's own family history shows (ownhealth.family/v1, under `patterns` in GET /pedigree).
	// Picking one rings the relatives it rests on in the pedigree card above; picking it again clears it.
	import type { FamilyPatterns } from '#lib/api/family';
	import { cap } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';

	let {
		patterns,
		relLabel,
		kindLabel,
		selected = $bindable(null)
	}: {
		patterns: FamilyPatterns;
		relLabel: (code: string) => string;
		kindLabel: (kind: string) => string;
		selected?: string | null;
	} = $props();

	const lang = $derived(i18n.lang as 'es' | 'en');
	// the relatives, then the conditions the pattern matched: "Tío paterno · padre · IAM"
	const who = (p: { rel: string[]; kinds: string[] }) =>
		[...[...new Set(p.rel)].map((r) => relLabel(r)), ...p.kinds.map((k) => kindLabel(k))].join(' · ');
	const pick = (rule: string) => (selected = selected === rule ? null : rule);
</script>

<section class="card" aria-labelledby="fam-pat-h">
	<h3 id="fam-pat-h">{t('fam_pat_t')}<span class="r">{t('fam_pat_r')}</span></h3>
	{#if !patterns.items.length}
		<div class="dim none">{t('fam_pat_none')}</div>
	{:else}
		<ul>
			{#each patterns.items as p (p.rule)}
				<li class="pat" class:on={selected === p.rule}>
					<button type="button" class="head" aria-pressed={selected === p.rule} title={t('fam_pat_shade')} onclick={() => pick(p.rule)}>
						<span class="dot" aria-hidden="true"></span><b>{p.title[lang]}</b>
						{#if p.status === 'not_assessed'}<span class="chip">{t('fam_pat_na')}</span>{/if}
					</button>
					<div class="why">{p.status === 'not_assessed' && p.reason ? p.reason[lang] : p.why[lang]}</div>
					<div class="meta">
						<span>{cap(who(p))}</span>
						<a class="chip" href={p.cite} target="_blank" rel="noopener" title={t('fam_pat_cite')}>{p.source}</a>
						{#if p.basis === 'pack_default'}<span class="chip" title={t('fam_pat_ours_d')}>{t('fam_pat_ours')}</span>{/if}
						<span class="mono">{p.rule}</span>
					</div>
				</li>
			{/each}
		</ul>
	{/if}
	<div class="dim note">{t('fam_pat_kind')}</div>
	<div class="mono dim rules">{patterns.rules}</div>
</section>

<style>
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.pat {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 6px 9px;
	}
	.pat.on {
		border-color: var(--watch);
		background: var(--watch-bg);
	}
	.head {
		background: none;
		border: 0;
		padding: 0;
		color: var(--text);
		font: inherit;
		font-size: 12.5px;
		text-align: left;
		cursor: pointer;
		display: flex;
		align-items: flex-start;
		gap: 7px;
		width: 100%;
	}
	.head b {
		flex: 1;
	}
	.head:hover b,
	.head:focus-visible b {
		color: var(--accent);
	}
	.head:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
		border-radius: 4px;
	}
	.dot {
		width: 10px;
		height: 10px;
		border-radius: 50%;
		border: 2px solid var(--watch);
		flex: none;
		margin-top: 4px;
	}
	.on .dot {
		background: var(--watch);
	}
	.why {
		font-size: 12px;
		color: var(--muted);
		margin-top: 2px;
	}
	.meta {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
		align-items: center;
		margin-top: 4px;
		font-size: 11px;
		color: var(--dim);
	}
	.meta a.chip {
		text-decoration: none;
		white-space: normal;
	}
	.mono {
		font-size: 10px;
	}
	.none,
	.note {
		font-size: 11.5px;
	}
	.note {
		margin-top: 6px;
	}
	.rules {
		font-size: 10.5px;
		margin-top: 4px;
	}
</style>
