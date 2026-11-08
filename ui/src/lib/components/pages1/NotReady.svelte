<script lang="ts">
	// Honest empty state for a card or page whose data did not arrive: says so, never fills in.
	import { t } from '#lib/i18n/index.svelte';
	import { errCode, tf } from './util.svelte';

	let { error, onretry }: { error: unknown; onretry?: () => void } = $props();
</script>

<div class="empty" role="status">
	<b>{t('h1_na')}</b><br />
	<span class="dim">{tf('h1_na_d', { code: errCode(error) })}</span>
	{#if onretry}<br /><button class="btn sm" style="margin-top:8px" onclick={onretry}>{t('retry')}</button>{/if}
</div>
