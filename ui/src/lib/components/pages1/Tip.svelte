<script lang="ts">
	// The mockup's floating tooltip (.tip), positioned next to the pointer.
	import type { Snippet } from 'svelte';

	let { x, y, children }: { x: number; y: number; children: Snippet } = $props();
</script>

<div class="tip" role="tooltip" style="display:block;left:{x + 14}px;top:{y + 12}px">{@render children()}</div>
