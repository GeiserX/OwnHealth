<script lang="ts" module>
	export interface Box {
		id: string;
		bbox: number[];
		tone: '' | 'low' | 'bad';
		label: string;
	}
</script>

<script lang="ts">
	// A document page as the backend renders it, with boxes over the rows extracted from it.
	// Boxes are in PDF points, top-left origin ([x0, top, x1, bottom], pdfplumber's convention).
	import { pageImageUrl } from '#lib/api';
	import type { Uuid } from '#lib/api/types';
	import { t } from '#lib/i18n/index.svelte';

	let {
		documentId,
		page,
		width,
		height,
		boxes,
		selected = null,
		zoom = 1,
		onpick
	}: { documentId: Uuid; page: number; width: number; height: number; boxes: Box[]; selected?: string | null; zoom?: number; onpick: (id: string) => void } = $props();

	let failed = $state(false);
	$effect(() => {
		void documentId;
		void page;
		failed = false;
	});
	const pct = (v: number, of: number) => `${((v / of) * 100).toFixed(2)}%`;
	const pad = 2; // pt around the row, like the mockup's outline
</script>

<div class="scroller">
<div class="pv" style="aspect-ratio:{width} / {height};width:{zoom * 100}%">
	{#if failed}
		<div class="empty" style="padding-top:40%">{t('i2_page_noimg')}</div>
	{:else}
		<img src={pageImageUrl(documentId, page)} alt="{t('i2_page')} {page}" onerror={() => (failed = true)} />
	{/if}
	<!-- The boxes point at rows the list beside the page already offers to the keyboard and screen readers,
	     so they are a second way in for the pointer only, with a finger-sized hit area on touch screens. -->
	{#each boxes as b (b.id)}
		{@const [x0, y0, x1, y1] = b.bbox}
		<button
			class="box {b.tone === 'bad' ? 'bb-x' : b.tone === 'low' ? 'bb-w' : 'bb'}"
			class:on={selected === b.id}
			style="left:{pct(x0 - pad, width)};top:{pct(y0 - pad, height)};width:{pct(x1 - x0 + 2 * pad, width)};height:{pct(y1 - y0 + 2 * pad, height)}"
			title={b.label}
			tabindex="-1"
			aria-hidden="true"
			onclick={() => onpick(b.id)}
		></button>
	{/each}
</div>
</div>

<style>
	.scroller {
		max-height: calc(100vh - 230px);
		overflow: auto;
		border-radius: 4px;
		box-shadow: 0 8px 30px rgba(0, 0, 0, 0.45);
	}
	.pv {
		position: relative;
		background: #f4f2ee;
		overflow: hidden;
	}
	.pv img {
		display: block;
		width: 100%;
		height: 100%;
	}
	.box {
		position: absolute;
		border: 0;
		padding: 0;
		border-radius: 1px;
	}
	@media (pointer: coarse) {
		.box::after {
			content: '';
			position: absolute;
			left: 0;
			right: 0;
			top: 50%;
			height: max(100%, 24px);
			transform: translateY(-50%);
		}
	}
	.box.on {
		outline-width: 3px;
		box-shadow: 0 0 0 4px rgba(110, 168, 254, 0.25);
	}
</style>
