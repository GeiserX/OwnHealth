<script lang="ts" module>
	// GET /api/genome/carrier-couple (routes/genome/couple.rs). The texts come from the server in the
	// reader's language (data/genome/couple.json), so they stay under the wording lint.
	export type CoupleSide = {
		state: 'carrier' | 'other_finding' | 'not_detected' | 'not_stated' | 'probed_only' | 'not_assessed' | 'no_genome';
		label: string;
		confirmed: boolean | null;
		assays: string[];
		zygosity: string | null;
		coverage: 'assessed' | 'partial' | 'not_stated' | 'not_assessed' | null;
		src: string[];
	};
	export type CoupleRow = {
		kind: string;
		gene: string | null;
		inheritance: 'AR' | 'XL' | null;
		condition: string | null;
		a: CoupleSide | null;
		b: CoupleSide | null;
		risk: string | null;
		text: string;
		counsel: string;
		cite: string | null;
	};
	export type CoupleView = {
		a: { profile: string; name: string };
		b: { profile: string; name: string };
		lang: 'es' | 'en';
		rules: string;
		rows: CoupleRow[];
		withheld: { sensitive: number };
		counsel: string;
		notice: string;
	};
</script>

<script lang="ts">
	// The couple carrier card of the Genome screen (genomics.md section 10): two adult partners' carrier
	// facts joined by gene. A risk figure shows only when the server gives one (both sides confirmed).
	import { ApiError, get } from '#lib/api/core';
	import type { ProfileSummary } from '#lib/api/types';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { app } from '#lib/state/app.svelte';

	// `rev` changes when the page reloads its genome data (a consent change): the card fetches again and
	// keeps the chosen partner.
	let { profile, rev }: { profile: ProfileSummary; rev?: unknown } = $props();

	// Partners the caller can join with this profile: other adults whose genome the caller may read.
	const candidates = $derived((app.me?.profiles ?? []).filter((p) => p.slug !== profile.slug && !p.is_minor && p.scopes.includes('read:genome')));
	// The partner relationship first: from the household's own profile its partner, and the other way round.
	const preferred = $derived(
		candidates.find((p) => (profile.relationship === 'partner' ? p.relationship === 'self' : p.relationship === 'partner')) ?? candidates[0] ?? null
	);
	let chosen = $state<string | null>(null);
	const partner = $derived(candidates.find((p) => p.slug === chosen) ?? preferred);
	const usable = $derived(!profile.is_minor && profile.scopes.includes('read:genome'));

	const view = new Loader<CoupleView>();
	$effect(() => {
		const a = profile.slug;
		const b = partner?.slug;
		const lang = i18n.lang;
		void rev;
		if (!usable || !b) return;
		view.run(() => get<CoupleView>('/genome/carrier-couple', { a, b, lang }), `${a}/${b}/${lang}`);
	});

	const v = $derived(partner ? view.data : null);
	// A deceased or minor profile is refused by design (the profile list does not say who is deceased).
	const ineligible = $derived(view.error instanceof ApiError && view.error.code === 'couple_not_eligible');
	const tone = (s: CoupleSide) =>
		s.state === 'carrier' ? (s.confirmed ? 'p-watch' : 'p-stale') : s.state === 'other_finding' ? 'p-watch' : s.state === 'not_detected' ? 'p-info' : 'p-stale';
	// The citation's site, so a long guideline URL does not fill the row; the title holds the whole URL.
	const host = (u: string) => {
		try {
			return new URL(u).hostname.replace(/^www\./, '');
		} catch {
			return u;
		}
	};
	const kindTone = (k: string) => (k === 'ar_both' ? 'p-out' : k === 'ar_both_unconfirmed' || k === 'ar_both_no_figure' || k.startsWith('xl') ? 'p-watch' : 'p-stale');
</script>

{#if usable}
	<section class="card couple" aria-labelledby="couple-h">
		<h3 id="couple-h">
			{t('gen_couple_t')}
			<span class="r">
				{#if candidates.length}
					<label class="dim" for="couple-with">{t('gen_couple_with')}</label>
					<select id="couple-with" class="p2in" value={partner?.slug ?? ''} onchange={(e) => (chosen = e.currentTarget.value)}>
						{#each candidates as c (c.slug)}<option value={c.slug}>{c.display_name}</option>{/each}
					</select>
				{/if}
			</span>
		</h3>
		<div class="muted intro">{t('gen_couple_d')}</div>

		{#if !partner}
			<div class="empty">{t('gen_couple_nobody')}</div>
		{:else if ineligible}
			<div class="empty">{t('gen_couple_not_eligible')}</div>
		{:else if view.error}
			<NotReady error={view.error} />
		{:else if view.loading && !v}
			<div class="empty">{t('loading')}</div>
		{:else if v}
			{#if !v.rows.length}
				<div class="empty">{t('gen_couple_none')}</div>
			{:else}
				<ul class="rows">
					{#each v.rows as r, i (r.gene ?? `withheld-${i}`)}
						<li class:withheld={!r.gene}>
							<div class="head">
								{#if r.gene}
									<b>{r.gene}</b>
									{#if r.condition}<span class="muted">{r.condition}</span>{/if}
									{#if r.inheritance}<span class="chip" title={t('gen_couple_inh_' + r.inheritance)}>{t('gen_couple_inh_' + r.inheritance)}</span>{/if}
									<span class="pill {kindTone(r.kind)}"><i aria-hidden="true"></i>{r.risk ? t('gen_couple_risk') : t('gen_couple_k_' + r.kind)}</span>
								{:else}
									<span class="muted"><span aria-hidden="true">{'🔒 '}</span>{t('gen_couple_withheld')}</span>
								{/if}
							</div>
							<p class="txt">{r.text}</p>
							{#if r.a && r.b}
								<div class="sides">
									{#each [[v.a.name, r.a], [v.b.name, r.b]] as const as [name, s]}
										<div class="cside">
											<span class="who">{name}</span>
											<span class="pill {tone(s)}"><i aria-hidden="true"></i>{s.label}</span>
											{#if s.assays.length}<span class="dim mono">{s.assays.map((x) => x.toUpperCase()).join(' · ')}</span>{/if}
											{#each s.src as src (src)}<SourceChip label="OwnGenome" {src} onopen={openSource} />{/each}
										</div>
									{/each}
								</div>
							{/if}
							{#if r.cite}<a class="cite dim" href={r.cite} target="_blank" rel="noopener" title={r.cite}>{t('gen_couple_cite')}: {host(r.cite)}</a>{/if}
						</li>
					{/each}
				</ul>
			{/if}
			<div class="note counsel">{v.counsel}</div>
		{/if}
	</section>
{/if}

<style>
	.couple {
		margin-top: 12px;
	}
	.couple h3 .r {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-weight: 400;
		font-size: 12px;
	}
	.intro {
		font-size: 12px;
		margin-bottom: 8px;
	}
	.rows {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.rows li {
		padding: 10px 0;
		border-top: 1px solid var(--line);
	}
	.head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 8px;
	}
	.txt {
		margin: 6px 0;
		font-size: 12.5px;
	}
	.sides {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 6px 16px;
	}
	.cside {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 8px;
		font-size: 12px;
		min-width: 0;
	}
	.cside .pill {
		white-space: normal;
	}
	.who {
		font-weight: 600;
	}
	.cite {
		display: inline-block;
		margin-top: 4px;
		font-size: 11px;
	}
	.counsel {
		margin-top: 8px;
		color: var(--text);
	}
	@media (max-width: 760px) {
		.sides {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
