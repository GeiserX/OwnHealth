<script lang="ts">
	// Honest empty state for a card or page whose data did not arrive: says so, never fills in.
	// A scope denial is a fixed fact, not an outage: it says the view is not shared and offers no retry.
	// An ended session is not an outage either: it offers the sign-in, never a retry that cannot work.
	import { ApiError } from '#lib/api/core';
	import { t } from '#lib/i18n/index.svelte';
	import { app } from '#lib/state/app.svelte';
	import { errCode, tf } from './util.svelte';

	let { error, onretry }: { error: unknown; onretry?: () => void } = $props();
	const denied = $derived(error instanceof ApiError && error.status === 403 && ['insufficient_scope', 'forbidden', 'profile_not_granted'].includes(error.code));
	const ended = $derived(error instanceof ApiError && error.status === 401);
</script>

<div class="empty" role="status">
	{#if denied}
		<b>🔒 {t('h1_noaccess')}</b><br />
		<span class="dim">{t('h1_noaccess_d')}</span>
	{:else if ended}
		<b>{t('session_t')}</b><br />
		<span class="dim">{t('session_d')}</span><br />
		<button class="btn sm pri" style="margin-top:8px" onclick={() => (app.unauthenticated = true)}>{t('session_btn')}</button>
	{:else}
		<b>{t('h1_na')}</b><br />
		<span class="dim">{tf('h1_na_d', { code: errCode(error) })}</span>
		{#if onretry}<br /><button class="btn sm" style="margin-top:8px" onclick={onretry}>{t('retry')}</button>{/if}
	{/if}
</div>
