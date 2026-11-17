<script lang="ts">
	// Consent per relative for cross-profile hints (v0.3, GET/PUT /profiles/{p}/family-shares): for each
	// other profile of the household, whether this profile's lab results and genetic findings may feed a
	// cited hint there. Only the profile's manager sees this card. Adding a category asks to confirm it is
	// you (step-up); taking one away does not. A minor's profile shares nothing, and a genetic finding
	// never feeds a hint in a minor's profile.
	import { listFamilyShares, putFamilyShare, type FamilyShares, type ShareCategory } from '#lib/api/family';
	import type { ProfileSummary } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf } from '#lib/components/pages1/util.svelte';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { t } from '#lib/i18n/index.svelte';

	let { profile }: { profile: ProfileSummary } = $props();
	const shares = new Loader<FamilyShares>();
	const load = () => shares.run(() => listFamilyShares(profile.slug), profile.slug);
	$effect(() => {
		load();
	});
	const first = $derived(profile.display_name.split(' ')[0]);
	// the row being saved, and the error of the last row that failed
	let busy = $state('');
	let error = $state<{ target: string; text: string; raw: string } | null>(null);

	type Item = FamilyShares['items'][number];
	function toggle(item: Item, cat: ShareCategory, on: boolean) {
		const next = (shares.data?.categories ?? []).filter((c) => (c === cat ? on : item.categories.includes(c)));
		busy = item.profile;
		error = null;
		withStepUp(async () => {
			const r = await putFamilyShare(profile.slug, item.profile, next);
			item.categories = r.categories;
		})
			.catch((e) => (error = { target: item.profile, text: errText(e), raw: errRaw(e) }))
			.finally(() => {
				busy = '';
				load();
			});
	}
	const degree = (d: number | null) => (d === 1 ? t('fam_share_deg1') : d ? tf('fam_share_degn', { n: d }) : t('fam_share_nodeg'));
</script>

<section class="card" aria-labelledby="fam-share-h">
	<h3 id="fam-share-h">{tf('fam_share_t', { name: first })}<span class="r">{t('fam_share_r')}</span></h3>
	{#if shares.error && !shares.data}
		<NotReady error={shares.error} onretry={load} />
	{:else if !shares.data}
		<div class="empty">{t('loading')}</div>
	{:else if shares.data.minor}
		<div class="dim none">{tf('fam_share_minor', { name: first })}</div>
	{:else if !shares.data.items.length}
		<div class="dim none">{t('fam_share_none')}</div>
	{:else}
		<div class="note">{tf('fam_share_d', { name: first })}</div>
		<ul>
			{#each shares.data.items as item (item.profile)}
				<li>
					<div class="who">
						<b>{item.display_name}</b>
						<span class="dim">{degree(item.degree)}</span>
					</div>
					<div class="cats" role="group" aria-label={tf('fam_share_with', { name: item.display_name })}>
						<label>
							<input type="checkbox" checked={item.categories.includes('labs')} disabled={busy !== ''} onchange={(e) => toggle(item, 'labs', e.currentTarget.checked)} />
							{t('fam_share_labs')}
						</label>
						<label class:off={item.minor}>
							<input
								type="checkbox"
								checked={item.categories.includes('cascade')}
								disabled={busy !== '' || (item.minor && !item.categories.includes('cascade'))}
								aria-describedby={item.minor ? `fam-share-minor-${item.profile}` : undefined}
								onchange={(e) => toggle(item, 'cascade', e.currentTarget.checked)}
							/>
							{t('fam_share_cascade')}
						</label>
						{#if item.minor}<span class="dim small" id="fam-share-minor-{item.profile}">{t('fam_share_adults')}</span>{/if}
					</div>
					{#if busy === item.profile}<div class="dim small" role="status">{t('fam_share_saving')}</div>{/if}
					{#if error?.target === item.profile}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
				</li>
			{/each}
		</ul>
		<div class="note warn">{tf('fam_share_gene', { name: first })}</div>
	{/if}
</section>

<style>
	h3 .r {
		white-space: normal;
	}
	ul {
		list-style: none;
		margin: 6px 0 0;
		padding: 0;
	}
	li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 6px 12px;
		padding: 7px 0;
		border-top: 1px solid var(--line);
	}
	.who {
		display: flex;
		flex-direction: column;
		min-width: 0;
		font-size: 12.5px;
	}
	.who .dim {
		font-size: 11.5px;
	}
	.cats {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 14px;
		font-size: 12.5px;
	}
	.cats label {
		display: flex;
		align-items: center;
		gap: 6px;
		cursor: pointer;
	}
	.cats label.off {
		color: var(--dim);
	}
	li > .small,
	li > .p2err {
		flex-basis: 100%;
	}
	.small {
		font-size: 11px;
	}
	.none,
	.note {
		font-size: 11.5px;
	}
	.note.warn {
		margin-top: 8px;
		color: var(--watch);
	}
</style>
