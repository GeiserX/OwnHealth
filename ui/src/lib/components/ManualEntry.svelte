<script module lang="ts">
	export type EntryKind = 'observation' | 'medication' | 'condition' | 'allergy' | 'immunization';
</script>

<script lang="ts">
	// Enter a value or a record by hand (observation, current medication, condition, allergy, vaccine),
	// or correct a record: a correction is stored as a new version with a reason, the old one stays.
	import '#lib/components/pages2/pages2.css';
	import { untrack } from 'svelte';
	import { addManualObservation, correctRecord, createRecord } from '#lib/api';
	import type { RecordOut } from '#lib/api/types';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { errText } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';

	type Fields = Record<string, string>;
	let {
		profile,
		kinds = ['observation', 'medication', 'condition', 'allergy', 'immunization'],
		correcting = null,
		note = '',
		onsaved,
		oncancel
	}: {
		profile: string;
		kinds?: EntryKind[];
		correcting?: RecordOut | null;
		note?: string;
		onsaved: (kind: EntryKind) => void;
		oncancel?: () => void;
	} = $props();

	// The fields each kind asks for, in order: [field, dict key, input type, required].
	const FIELDS: Record<EntryKind, [string, string, string, boolean][]> = {
		observation: [['name', 'me_f_name', 'text', true], ['value', 'me_f_value', 'text', true], ['unit', 'me_f_unit', 'text', false], ['date', 'me_f_date', 'date', true]],
		medication: [['name', 'me_f_med', 'text', true], ['strength', 'me_f_strength', 'text', false], ['dose_text', 'me_f_dose', 'text', false], ['started', 'me_f_start', 'date', false], ['stopped', 'me_f_stop', 'date', false]],
		condition: [['text', 'me_f_cond', 'text', true], ['onset', 'me_f_onset', 'date', false], ['abatement', 'me_f_abate', 'date', false]],
		allergy: [['substance', 'me_f_subst', 'text', true], ['reaction_text', 'me_f_reaction', 'text', false]],
		immunization: [['vaccine', 'me_f_vaccine', 'text', true], ['occurred', 'me_f_occurred', 'date', false], ['dose_number', 'me_f_dosen', 'number', false]]
	};
	const today = new Date().toISOString().slice(0, 10);
	const fresh = (k: EntryKind): Fields => (k === 'observation' ? { date: today, note } : {});
	const fromRecord = (r: RecordOut): Fields =>
		Object.fromEntries(FIELDS[r.kind as EntryKind].map(([f]) => [f, r.detail[f] == null ? '' : String(r.detail[f])]));

	// The form starts from the props once; a parent that needs a fresh form remounts it ({#key}).
	const start = untrack(() => (correcting ? { kind: correcting.kind as EntryKind, f: fromRecord(correcting) } : { kind: kinds[0], f: fresh(kinds[0]) }));
	let kind = $state<EntryKind>(start.kind);
	let f = $state<Fields>(start.f);
	let reason = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let saved = $state(0);

	function pick(k: EntryKind) {
		kind = k;
		f = fresh(k);
		error = null;
	}
	const val = (k: string) => (f[k] ?? '').trim() || null;
	function body(): Record<string, unknown> {
		const out: Record<string, unknown> = {};
		for (const [k, , type] of FIELDS[kind]) out[k] = type === 'number' ? (val(k) ? Number(val(k)) : null) : val(k);
		if (kind === 'medication') out.med_status = val('stopped') && val('stopped')! <= today ? 'stopped' : 'active';
		if (kind === 'condition') out.clinical_status = val('abatement') ? 'resolved' : 'active';
		return out;
	}
	async function save(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		error = null;
		try {
			if (correcting) {
				await correctRecord(profile, correcting.kind, correcting.id, { ...correcting.detail, ...body(), reason: reason.trim() });
			} else if (kind === 'observation') {
				await addManualObservation(profile, {
					name: val('name')!,
					value: val('value')!,
					unit: val('unit'),
					effective_at: `${val('date') ?? today}T12:00:00Z`,
					note: val('note')
				});
			} else {
				await createRecord(profile, kind, body());
			}
			saved++;
			const k = kind;
			if (!correcting) f = { ...fresh(k), ...(k === 'observation' ? { date: f.date } : {}) };
			onsaved(k);
		} catch (err) {
			error = errText(err);
		} finally {
			busy = false;
		}
	}
</script>

<form class="p2form" onsubmit={save} use:focusOnOpen={{ oncancel }} aria-label={correcting ? t('me_correct') : t('me_add')}>
	{#if !correcting && kinds.length > 1}
		<div class="seg" role="group" aria-label={t('me_kind')}>
			{#each kinds as k (k)}<button type="button" class:on={k === kind} aria-pressed={k === kind} onclick={() => pick(k)}>{t('me_k_' + k)}</button>{/each}
		</div>
	{/if}
	<div class="p2row">
		{#each FIELDS[kind] as [k, key, type, req] (k)}
			<label style={type === 'text' ? 'flex:1;min-width:120px' : ''}>{t(key)}<input class="p2in" {type} bind:value={f[k]} required={req} /></label>
		{/each}
	</div>
	{#if kind === 'observation' && !correcting}
		<label>{t('me_f_note')}<input class="p2in" bind:value={f.note} maxlength="4000" /></label>
	{/if}
	{#if correcting}
		<label>{t('me_reason')}<input class="p2in" bind:value={reason} required maxlength="500" /></label>
	{/if}
	{#if error}<div class="p2err" role="alert">{error}</div>{/if}
	<div class="p2row">
		<button class="btn pri sm" type="submit" disabled={busy}>{correcting ? t('me_save_corr') : t('me_save')}</button>
		{#if oncancel}<button class="btn sm" type="button" onclick={oncancel}>{t('i2_cancel')}</button>{/if}
		{#if saved && !correcting}<span class="dim" style="font-size:11.5px" aria-live="polite">{t('me_saved')}</span>{/if}
	</div>
	<div class="note">{t('me_note')}</div>
</form>
