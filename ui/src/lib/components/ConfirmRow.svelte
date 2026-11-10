<script lang="ts">
	// The second step of every destructive or access-widening action: one line that says exactly what
	// happens, Cancel (it takes focus, so Enter or Escape is never the risky choice) and the red confirm
	// button. Never window.confirm: it breaks the dark theme and speaks the browser's language.
	import { onMount } from 'svelte';
	import { t } from '#lib/i18n/index.svelte';

	let {
		text,
		confirmLabel,
		onconfirm,
		oncancel,
		busy = false
	}: { text: string; confirmLabel: string; onconfirm: () => void; oncancel: () => void; busy?: boolean } = $props();
	let cancelEl: HTMLButtonElement | undefined = $state();
	onMount(() => cancelEl?.focus());
	function onKey(e: KeyboardEvent) {
		if (e.key !== 'Escape') return;
		e.preventDefault();
		e.stopPropagation();
		oncancel();
	}
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div class="confirm" role="group" aria-label={text} onkeydown={onKey}>
	<span class="txt">{text}</span>
	<span class="acts">
		<button type="button" class="btn sm" bind:this={cancelEl} onclick={oncancel}>{t('i2_cancel')}</button>
		<button type="button" class="btn sm no" disabled={busy} onclick={onconfirm}>{confirmLabel}</button>
	</span>
</div>

<style>
	.confirm {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 12px;
		padding: 8px 10px;
		border: 1px solid rgba(239, 107, 115, 0.4);
		border-radius: 8px;
		background: var(--out-bg);
		font-size: 12px;
	}
	.txt {
		flex: 1 1 220px;
		min-width: 0;
	}
	.acts {
		display: flex;
		gap: 6px;
		margin-left: auto;
	}
</style>
