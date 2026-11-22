<script lang="ts">
	// What a FIT activity file brought (job import.fit, slice 29), under its row in the upload panel:
	// heart rate minutes, distances and energy imported, what was left out and why, and that its
	// positions were not stored. Lines from #lib/logic/fit_import, words from the dictionary.
	import { tf, tp } from '#lib/components/pages1/util.svelte';
	import type { FitLine } from '#lib/logic/fit_import';

	let { lines, label }: { lines: FitLine[]; label: string } = $props();
	const text = (l: FitLine) => (l.n != null ? tp(l.key, l.n, l.vars) : tf(l.key, l.vars ?? {}));
</script>

<ul class="fit-lines" aria-label={label}>
	{#each lines as l}<li class:dim={l.n === 0 || l.key.startsWith('i2_fit_r_')}>{text(l)}</li>{/each}
</ul>

<style>
	.fit-lines {
		margin: 0;
		padding: 0 0 4px 0;
		list-style: none;
		font-size: 12px;
		overflow-wrap: anywhere;
	}
	.fit-lines li + li {
		margin-top: 2px;
	}
</style>
