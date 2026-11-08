<script lang="ts">
	// Honest empty state for a card or page whose data did not arrive: says so, never fills in.
	// A scope denial is a fixed fact, not an outage: it says the view is not shared and offers no retry.
	import { ApiError } from '#lib/api/core';
	import { t } from '#lib/i18n/index.svelte';
	import { errCode, tf } from './util.svelte';

	let { error, onretry }: { error: unknown; onretry?: () => void } = $props();
	const denied = $derived(error instanceof ApiError && error.status === 403 && ['insufficient_scope', 'forbidden', 'profile_not_granted'].includes(error.code));
</script>

<div class="empty" role="status">
	{#if denied}
		<b>🔒 {t('h1_noaccess')}</b><br />
		<span class="dim">{t('h1_noaccess_d')}</span>
	{:else}
		<b>{t('h1_na')}</b><br />
		<span class="dim">{tf('h1_na_d', { code: errCode(error) })}</span>
		{#if onretry}<br /><button class="btn sm" style="margin-top:8px" onclick={onretry}>{t('retry')}</button>{/if}
	{/if}
</div>
