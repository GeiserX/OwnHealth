<script lang="ts">
	// The mockup's `.sw` toggle as a real switch: a button with role="switch", its state in words too.
	import { t } from '#lib/i18n/index.svelte';

	// `words`: what the state reads as next to the switch, when 'on'/'off' is not the right word.
	let {
		on,
		label,
		disabled = false,
		words = null,
		onchange
	}: { on: boolean; label: string; disabled?: boolean; words?: [string, string] | null; onchange: (on: boolean) => void } = $props();
</script>

<!-- aria-disabled, not disabled: a focused button that turns disabled while its change saves drops
     keyboard focus to the page body, so the next Tab would start from the top. -->
<button type="button" class="swb" role="switch" aria-checked={on} aria-label={label} aria-disabled={disabled} onclick={() => !disabled && onchange(!on)}>
	<span class="sw" class:off={!on} aria-hidden="true"></span>
	<span class="dim" style="font-size:11px">{on ? (words?.[0] ?? t('i2_on')) : (words?.[1] ?? t('i2_off'))}</span>
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
	.swb[aria-disabled='true'] {
		opacity: 0.5;
		cursor: not-allowed;
	}
</style>
