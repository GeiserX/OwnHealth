<script lang="ts">
	// New personal access token for an MCP client: label, profiles, scopes, lifetime. The plaintext comes
	// back once; the parent shows it with the Claude Code snippet and never stores it.
	import { createToken } from '#lib/api';
	import type { ProfileSummary, Scope, TokenOut } from '#lib/api/types';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { errText } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { withStepUp } from './stepup.svelte';

	let { profiles, oncreated, oncancel }: { profiles: ProfileSummary[]; oncreated: (token: TokenOut, plaintext: string) => void; oncancel: () => void } =
		$props();

	const SCOPES: { s: Scope; tone: '' | 'w' | 'r' }[] = [
		{ s: 'read:summary', tone: '' },
		{ s: 'read:labs', tone: '' },
		{ s: 'read:vitals', tone: '' },
		{ s: 'read:wearables', tone: '' },
		{ s: 'read:records', tone: '' },
		{ s: 'read:genome', tone: '' },
		{ s: 'read:documents_text', tone: '' },
		{ s: 'read:genome_raw', tone: 'r' },
		{ s: 'read:sensitive', tone: 'r' },
		{ s: 'write:propose', tone: 'w' }
	];
	let label = $state('Claude Code');
	let chosen = $state<string[]>([]);
	let scopes = $state<Scope[]>(['read:summary', 'read:labs', 'read:vitals', 'read:records']);
	let days = $state(90);
	let identity = $state<'full' | 'pseudonymous' | 'minimal'>('pseudonymous');
	let error = $state<string | null>(null);
	let busy = $state(false);
	$effect(() => {
		if (!chosen.length) {
			const own = profiles.find((p) => p.is_self) ?? profiles[0];
			if (own) chosen = [own.slug];
		}
	});

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		error = null;
		if (!chosen.length || !scopes.length) {
			error = t('i2_tok_need');
			return;
		}
		busy = true;
		try {
			const res = await withStepUp(() => createToken({ label: label.trim(), profiles: chosen, scopes, days, identity_mode: identity, default_profile: chosen[0] }));
			oncreated(res.token, res.plaintext);
		} catch (err) {
			error = errText(err);
		} finally {
			busy = false;
		}
	}
</script>

<form class="card p2form" onsubmit={submit} use:focusOnOpen={{ oncancel }} aria-labelledby="tf-h">
	<h3 id="tf-h">{t('i2_tok_new')}<span class="r">{t('i2_tok_stepup')}</span></h3>
	<label>{t('i2_tok_label')}<input class="p2in" bind:value={label} required maxlength="80" /></label>
	<fieldset class="fs">
		<legend>{t('i2_tok_profiles')}</legend>
		{#each profiles as p (p.id)}
			<label class="ck"><input type="checkbox" value={p.slug} bind:group={chosen} /> {p.display_name}</label>
		{/each}
	</fieldset>
	<fieldset class="fs">
		<legend>{t('i2_tok_scopes')}</legend>
		{#each SCOPES as x (x.s)}
			<label class="ck"><input type="checkbox" value={x.s} bind:group={scopes} /> <span class="chip mono {x.tone}">{x.s}</span></label>
		{/each}
	</fieldset>
	<div class="p2row">
		<label>{t('i2_tok_days')}<input class="p2in num" type="number" min="1" max="365" required bind:value={days} style="width:90px" /></label>
		<label
			>{t('i2_tok_identity')}
			<select class="p2in" bind:value={identity}>
				<option value="pseudonymous">{t('i2_id_pseudonymous')}</option>
				<option value="minimal">{t('i2_id_minimal')}</option>
				<option value="full">{t('i2_id_full')}</option>
			</select>
		</label>
	</div>
	<div class="note">{t('i2_tok_note')}</div>
	{#if error}<div class="p2err" role="alert">{error}</div>{/if}
	<div class="p2row" style="justify-content:flex-end">
		<button type="button" class="btn" onclick={oncancel}>{t('i2_cancel')}</button>
		<button type="submit" class="btn pri" disabled={busy}>{t('i2_tok_create')}</button>
	</div>
</form>

<style>
	.fs {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 6px 10px 8px;
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
	}
	.fs legend {
		font-size: 11.5px;
		color: var(--muted);
		padding: 0 4px;
	}
	.ck {
		display: inline-flex !important;
		align-items: center;
		gap: 5px;
		font-size: 12.5px !important;
		color: var(--text) !important;
	}
	.chip.w {
		color: var(--watch);
		border-color: rgba(224, 168, 74, 0.5);
	}
	.chip.r {
		color: var(--out);
		border-color: rgba(239, 107, 115, 0.5);
	}
</style>
