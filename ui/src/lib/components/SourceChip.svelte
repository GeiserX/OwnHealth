<script lang="ts">
	// A value's provenance: 'LabNorte · 2026-09-14 · p2'. Click opens the source (page + box).
	import { t } from '#lib/i18n/index.svelte';

	let { label, src, onopen }: { label: string; src?: string | null; onopen?: (src: string) => void } = $props();
</script>

<button
	type="button"
	class="src"
	class:bare={!label}
	style="background:none;border:0;padding:0"
	title={`${t('open_src')}: ${label || t('source')}`}
	aria-label={`${t('open_src')}: ${label || t('source')}`}
	onclick={() => src && onopen?.(src)}
>
	<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h8l4 4v14H6z" /><path d="M14 3v4h4" /></svg>{label}
</button>

<style>
	/* A chip with no words is only its icon: still a 24 px target. */
	.bare {
		min-width: 24px;
		min-height: 24px;
		justify-content: center;
	}
</style>
