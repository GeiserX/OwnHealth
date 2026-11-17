<script lang="ts">
	// An address the app does not have, or a page that failed while it loaded: said in the page's language,
	// with the one way back. A failure (5xx) never claims the page does not exist. The status stays small,
	// for whoever reports it.
	import { page } from '$app/state';
	import { t } from '#lib/i18n/index.svelte';
</script>

<div class="card nf">
	{#if page.status >= 500}
		<h1>{t('nf_err')}</h1>
		<p class="dim hint">{t('nf_err_hint')}</p>
	{:else}
		<h1>{t('nf_t')}</h1>
	{/if}
	<a href="/">{t('nf_go')}</a>
	<div class="dim num">{page.status}</div>
</div>

<style>
	.nf {
		max-width: 460px;
		margin: 8vh auto 0;
		display: grid;
		gap: 8px;
		text-align: center;
		padding: 20px 18px;
	}
	h1 {
		font-size: 17px;
		font-weight: 620;
		margin: 0;
	}
	.dim {
		font-size: 11px;
	}
	.hint {
		font-size: 13px;
		margin: 0;
	}
</style>
