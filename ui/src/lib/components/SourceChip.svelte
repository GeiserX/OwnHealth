<script lang="ts">
	// A value's provenance: 'LabNorte · 2026-09-14 · p2'. Click opens the source (page + box).
	import { t } from '#lib/i18n/index.svelte';
	import { sourceLabel } from '#lib/logic/format';

	// `name` says what the chip is the source of, for a chip with no words (a medication's record).
	let { label: raw, name, src, onopen }: { label: string; name?: string; src?: string | null; onopen?: (src: string) => void } = $props();
	const label = $derived(sourceLabel(raw, t('src_manual')));
	const said = $derived(label || name || t('source'));
</script>

<button
	type="button"
	class="src"
	class:bare={!label}
	style="background:none;border:0;padding:0"
	title={`${t('open_src')}: ${said}`}
	aria-label={`${t('open_src')}: ${said}`}
	onclick={() => src && onopen?.(src)}
>
	<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h8l4 4v14H6z" /><path d="M14 3v4h4" /></svg><span class="lbl">{label}</span>
</button>

<style>
	/* A label longer than its place ends in an ellipsis; the title holds the whole of it. */
	.src {
		max-width: 100%;
	}
	.lbl {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	/* A chip with no words is only its icon: still a 24 px target. */
	.bare {
		min-width: 24px;
		min-height: 24px;
		justify-content: center;
	}
</style>
