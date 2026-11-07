<script lang="ts">
	import type { Snippet } from 'svelte';

	let { title, right, children, class: cls = '', style = '' }: {
		title?: string;
		right?: Snippet | string;
		children?: Snippet;
		class?: string;
		style?: string;
	} = $props();
</script>

<section class="card {cls}" {style}>
	{#if title || right}
		<h3>
			{title ?? ''}
			{#if right}<span class="r">{#if typeof right === 'string'}{right}{:else}{@render right()}{/if}</span>{/if}
		</h3>
	{/if}
	{@render children?.()}
</section>
