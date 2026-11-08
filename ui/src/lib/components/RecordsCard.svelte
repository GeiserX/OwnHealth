<script lang="ts">
	// The profile's records by kind, each with Correct (a new version with a reason) and Retract (with a
	// reason), and a form to add one by hand. Writing needs a role that may write (not viewer).
	import '#lib/components/pages2/pages2.css';
	import { listRecords, retractRecord } from '#lib/api';
	import type { RecordOut } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader } from '#lib/components/pages1/util.svelte';
	import ManualEntry, { type EntryKind } from './ManualEntry.svelte';
	import { t } from '#lib/i18n/index.svelte';

	let { profile, canWrite, onchange }: { profile: string; canWrite: boolean; onchange?: () => void } = $props();
	type RK = Exclude<EntryKind, 'observation'>;
	const KINDS: RK[] = ['medication', 'condition', 'allergy', 'immunization'];
	let kind = $state<RK>('medication');
	const recs = new Loader<RecordOut[]>();
	let adding = $state(false);
	let correcting = $state<string | null>(null);
	let retracting = $state<string | null>(null);
	let reason = $state('');
	let error = $state<string | null>(null);

	function load() {
		const p = profile;
		const k = kind;
		recs.run(async () => (await listRecords(p, k)).items);
	}
	$effect(load);
	function changed() {
		adding = false;
		correcting = null;
		retracting = null;
		load();
		onchange?.();
	}
	async function retract(e: SubmitEvent, r: RecordOut) {
		e.preventDefault();
		error = null;
		try {
			await retractRecord(profile, r.kind, r.id, reason.trim());
			reason = '';
			changed();
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
		}
	}
	const sub = (r: RecordOut) => {
		const d = r.detail;
		const parts = [d.dose_text, d.started && `${t('me_f_start')} ${d.started}`, d.stopped && `${t('me_f_stop')} ${d.stopped}`, d.onset, d.occurred, d.reaction_text];
		return parts.filter(Boolean).join(' · ');
	};
</script>

<div class="card">
	<h3>{t('me_records')}{#if canWrite}<span class="r"><button class="btn sm" onclick={() => ((adding = !adding), (correcting = null))} aria-expanded={adding}>{t('me_add')}</button></span>{/if}</h3>
	<div class="seg" role="group" aria-label={t('me_kind')} style="margin-bottom:6px">
		{#each KINDS as k (k)}<button class:on={k === kind} aria-pressed={k === kind} onclick={() => ((kind = k), (adding = false))}>{t('me_k_' + k)}</button>{/each}
	</div>
	{#if adding}
		{#key kind}<ManualEntry {profile} kinds={[kind]} onsaved={changed} oncancel={() => (adding = false)} />{/key}
	{/if}
	{#if error}<div class="p2err" role="alert">{error}</div>{/if}
	{#if recs.error}
		<NotReady error={recs.error} onretry={load} />
	{:else}
		{#each recs.data ?? [] as r (r.id)}
			<div style="padding:5px 0;border-top:1px solid #20252f">
				<div class="p2row">
					<div style="flex:1;min-width:0"><b>{r.title}</b>{#if sub(r)}<div class="dim" style="font-size:11px">{sub(r)}</div>{/if}</div>
					{#if canWrite}
						<button class="btn sm" onclick={() => ((correcting = correcting === r.id ? null : r.id), (retracting = null))} aria-expanded={correcting === r.id}>{t('me_correct')}</button>
						<button class="btn sm no" onclick={() => ((retracting = retracting === r.id ? null : r.id), (correcting = null), (reason = ''))} aria-expanded={retracting === r.id}>{t('me_retract')}</button>
					{/if}
				</div>
				{#if correcting === r.id}
					<ManualEntry {profile} correcting={r} onsaved={changed} oncancel={() => (correcting = null)} />
				{:else if retracting === r.id}
					<form class="p2row" style="margin-top:4px" onsubmit={(e) => retract(e, r)}>
						<input class="p2in" style="flex:1" bind:value={reason} required maxlength="500" placeholder={t('me_reason')} aria-label={t('me_reason')} />
						<button class="btn sm no" type="submit">{t('me_retract')}</button>
					</form>
				{/if}
			</div>
		{:else}
			<div class="dim" style="font-size:12px">{recs.loading ? t('loading') : t('me_none')}</div>
		{/each}
	{/if}
</div>
