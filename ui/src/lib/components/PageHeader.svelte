<script lang="ts">
	import type { Snippet } from 'svelte';

	let { title, sub, right }: { title: string; sub?: string; right?: Snippet } = $props();
</script>

<div class="ph">
	<div>
		<h1>{title}</h1>
		{#if sub}<div class="sub">{sub}</div>{/if}
	</div>
	{#if right}<div class="right">{@render right()}</div>{/if}
</div>
