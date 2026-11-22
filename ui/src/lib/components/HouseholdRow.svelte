<script lang="ts">
	// One member in the home page's household overview: flags, overdue items and stale values from the
	// member's own status and Due list, as the signed-in person's grant lets them see it. A count is
	// never coloured alone: the column header names it, and a zero the grant may hide says so.
	import { ApiError } from '#lib/api';
	import type { ProfileSummary } from '#lib/api/types';
	import { C } from '#lib/components/colors';
	import { errCode, tf } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import type { MemberCounts } from '#lib/logic/household';

	let {
		profile,
		color,
		current,
		counts,
		error,
		onopen
	}: { profile: ProfileSummary; color: string; current: boolean; counts: MemberCounts | null; error: unknown; onopen: (slug: string) => void } = $props();

	// A grant without the summary answers 403: the row is not shared, not empty.
	const locked = $derived(error instanceof ApiError && error.status === 403);
	const cell = (n: number, c: string) => (n ? `color:${c};font-weight:600` : '');
</script>

<tr class:on={current}>
	<td>
		<button class="who" type="button" aria-label={tf('hh_open', { name: profile.display_name })} aria-current={current ? 'true' : undefined} onclick={() => onopen(profile.slug)}>
			<span class="av" style="background:{color}" aria-hidden="true">{profile.initials}</span><span>{profile.display_name}</span>
		</button>
	</td>
	{#if counts}
		<td class="num" style={cell(counts.flags, C.out)}>{counts.flags}</td>
		<td class="num" style={cell(counts.overdue, C.out)}>{counts.overdue}</td>
		<td class="num" style={cell(counts.stale, C.stale)}>{counts.stale}</td>
		<td class="hh-left">
			{#if counts.withheld.length}<span class="dim" title={tf('h1_withheld_sys', { scope: counts.withheld.join(', ') })}>🔒 {tf('hh_partial', { scope: counts.withheld.join(', ') })}</span>{/if}
		</td>
	{:else}
		<td class="num dim">—</td>
		<td class="num dim">—</td>
		<td class="num dim">—</td>
		<td class="hh-left">
			{#if locked}<span class="chip" title={tf('h1_withheld_sys', { scope: 'read:summary' })}>🔒 {t('h1_locked')}</span>
			{:else if error}<span class="dim">{errCode(error)}</span>
			{:else}<span class="dim">{t('loading')}</span>{/if}
		</td>
	{/if}
</tr>

<style>
	td {
		padding: 5px 8px;
		border-top: 1px solid var(--line);
		font-size: 12.5px;
		vertical-align: middle;
	}
	td.num {
		text-align: right;
		width: 1%;
		white-space: nowrap;
	}
	td.hh-left {
		font-size: 11.5px;
	}
	tr.on td {
		background: rgba(110, 168, 254, 0.06);
	}
	.who {
		display: flex;
		align-items: center;
		gap: 8px;
		background: none;
		border: 0;
		padding: 0;
		color: inherit;
		font: inherit;
		cursor: pointer;
		text-align: left;
	}
	.who:hover span:last-child {
		text-decoration: underline;
	}
	.av {
		width: 22px;
		height: 22px;
		border-radius: 50%;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		font-size: 10px;
		font-weight: 700;
		color: #0b0d12;
		flex: none;
	}
</style>
