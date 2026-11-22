<script lang="ts">
	// The language picker's buttons, one per registry language (langs.ts). The caller wraps them in
	// its `.seg` group; a beta language says so in its button and its tooltip.
	import { i18n, setLang } from '#lib/i18n/index.svelte';
	import { LANGS } from '#lib/i18n/langs';
</script>

{#each LANGS as l (l.code)}
	<button lang={l.code} class:on={i18n.lang === l.code} aria-pressed={i18n.lang === l.code} title={l.beta ? `${l.name} (beta)` : l.name} onclick={() => setLang(l.code)}>{l.label}{#if l.beta}<small> beta</small>{/if}</button>
{/each}
