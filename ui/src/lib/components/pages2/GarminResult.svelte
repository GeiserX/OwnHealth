<script lang="ts">
	import { tick } from 'svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { tp } from '#lib/components/pages1/util.svelte';
	import type { ActivityExportSummary } from '#lib/logic/garmin_import';

	let { summary, readings = null }: { summary: ActivityExportSummary; readings?: { key: string; n: number } | null } = $props();
	const pageSize = 50;
	let shown = $state(pageSize);
	let list = $state<HTMLUListElement>();

	// The next page of files; focus moves to its first file, as the button may leave with the last page.
	async function more() {
		const first = shown;
		shown += pageSize;
		await tick();
		(list?.children[first] as HTMLElement | undefined)?.focus();
	}
</script>

<div class="report">
	<div>{tp('i2_garmin_imported', summary.files_imported)}</div>
	{#if readings}<div>{tp(readings.key, readings.n)}</div>{/if}
	{#if summary.gps_left_out}<div class="dim">{t('i2_garmin_gps')}</div>{/if}
	{#if summary.rows_without_file}
		<div class="dim">{tp('i2_strava_rows_without_file', summary.rows_without_file)}</div>
	{/if}
	{#if summary.files_without_row}
		<div class="dim">{tp('i2_strava_files_without_row', summary.files_without_row)}</div>
	{/if}
	{#if summary.not_imported.length}
		<details>
			<summary>{tp('i2_garmin_omitted', summary.not_imported.length)}</summary>
			<ul bind:this={list}>
				{#each summary.not_imported.slice(0, shown) as item}
					<li tabindex="-1"><span dir="auto">{item.member}</span><span class="dim">{' · '}{t('i2_garmin_reason_' + item.reason)}</span></li>
				{/each}
			</ul>
			{#if shown < summary.not_imported.length}
				<button class="btn sm" onclick={more}>{t('i2_garmin_more')}</button>
			{/if}
		</details>
	{/if}
</div>

<style>
	.report {
		padding: 6px 0;
		font-size: 12px;
		overflow-wrap: anywhere;
		text-align: start;
	}
	details {
		margin-top: 4px;
	}
	summary {
		cursor: pointer;
	}
	ul {
		margin: 4px 0;
		padding-inline-start: 20px;
	}
	li + li {
		margin-top: 2px;
	}
</style>
