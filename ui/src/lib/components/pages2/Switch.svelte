<script lang="ts">
	// The mockup's `.sw` toggle as a real switch: a button with role="switch", its state in words too.
	import { t } from '#lib/i18n/index.svelte';

	let { on, label, disabled = false, onchange }: { on: boolean; label: string; disabled?: boolean; onchange: (on: boolean) => void } = $props();
</script>

<button type="button" class="swb" role="switch" aria-checked={on} aria-label={label} {disabled} onclick={() => onchange(!on)}>
	<span class="sw" class:off={!on} aria-hidden="true"></span>
	<span class="dim" style="font-size:11px">{on ? t('i2_on') : t('i2_off')}</span>
</button>

<style>
	.swb {
		background: none;
		border: 0;
		padding: 2px;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		border-radius: 10px;
	}
	.swb:disabled {
		opacity: 0.5;
		cursor: default;
	}
</style>
