<script lang="ts" module>
	export type GarminSummary = {
		files_imported: number;
		gps_left_out: boolean;
		not_imported: { member: string; reason: string }[];
	};

	export function garminSummary(value: unknown): GarminSummary | null {
		const summary = value as Partial<GarminSummary> | null;
		if (typeof summary?.files_imported !== 'number' || !Array.isArray(summary.not_imported)) return null;
		return {
			files_imported: summary.files_imported,
			gps_left_out: summary.gps_left_out === true,
			not_imported: summary.not_imported
		};
	}
</script>

<script lang="ts">
	import { t } from '#lib/i18n/index.svelte';
	import { tp } from '#lib/components/pages1/util.svelte';

	let { summary }: { summary: GarminSummary } = $props();
	const pageSize = 50;
	let shown = $state(pageSize);
</script>

<div class="report">
	<div>{tp('i2_garmin_imported', summary.files_imported)}</div>
	{#if summary.gps_left_out}<div class="dim">{t('i2_garmin_gps')}</div>{/if}
	{#if summary.not_imported.length}
		<details>
			<summary>{tp('i2_garmin_omitted', summary.not_imported.length)}</summary>
			<ul>
				{#each summary.not_imported.slice(0, shown) as item}
					<li><span dir="auto">{item.member}</span><span class="dim"> · {t('i2_garmin_reason_' + item.reason)}</span></li>
				{/each}
			</ul>
			{#if shown < summary.not_imported.length}
				<button class="btn sm" onclick={() => (shown += pageSize)}>{t('i2_garmin_more')}</button>
			{/if}
		</details>
	{/if}
</div>

<style>
	.report {
		padding: 6px 0;
		font-size: 12px;
		overflow-wrap: anywhere;
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
