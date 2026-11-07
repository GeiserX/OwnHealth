<script lang="ts">
	// A value's provenance: 'LabNorte · 2026-09-14 · p2'. Click opens the source (page + box).
	import { t } from '#lib/i18n/index.svelte';

	let { label, src, onopen }: { label: string; src?: string | null; onopen?: (src: string) => void } = $props();
</script>

<button
	type="button"
	class="src"
	style="background:none;border:0;padding:0"
	title={`${t('open_src')}: ${label}`}
	onclick={() => src && onopen?.(src)}
>
	<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h8l4 4v14H6z" /><path d="M14 3v4h4" /></svg>{label}
</button>
