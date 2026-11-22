<script lang="ts">
	// One member in the home page's household overview: flags from the member's status, overdue items
	// from the Due list and stale values from the member's dashboard, as the signed-in person's grant
	// lets them see it. A count is never coloured alone: the column header names it, and a zero the
	// grant may hide says so, under the name, where it wraps on a phone.
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
		onopen,
		onretry
	}: {
		profile: ProfileSummary;
		color: string;
		current: boolean;
		counts: MemberCounts | null;
		error: unknown;
		onopen: (slug: string) => void;
		onretry: (slug: string) => void;
	} = $props();

	// A grant without the summary answers 403, and a grant revoked while the page is open answers
	// profile_not_granted: the row is not shared, not empty and not an outage.
	const locked = $derived(error instanceof ApiError && (error.status === 403 || error.code === 'profile_not_granted'));
	const cell = (n: number, c: string) => (n ? `color:${c};font-weight:600` : '');
</script>

<tr class:on={current}>
	<td>
		<button class="who" type="button" aria-label={tf('hh_open', { name: profile.display_name })} aria-current={current ? 'true' : undefined} onclick={() => onopen(profile.slug)}>
			<span class="av" style="background:{color}" aria-hidden="true">{profile.initials}</span><span>{profile.display_name}</span>
		</button>
		{#if counts}
			{#if counts.withheld.length}<div class="note dim" title={tf('h1_withheld_sys', { scope: counts.withheld.join(', ') })}><span aria-hidden="true">🔒</span> {tf('hh_partial', { scope: counts.withheld.join(', ') })}</div>
			{:else if counts.noData}<div class="note dim">{t('h1_nomark')}</div>{/if}
		{:else if locked}<div class="note"><span class="chip" title={tf('h1_withheld_sys', { scope: 'read:summary' })}><span aria-hidden="true">🔒</span> {t('h1_locked')}</span></div>
		{:else if error}
			<div class="note" role="status">
				<b>{t('h1_na')}</b> <span class="dim">{tf('h1_na_d', { code: errCode(error) })}</span>
				<button class="btn sm" type="button" onclick={() => onretry(profile.slug)}>{t('retry')}</button>
			</div>
		{:else}<div class="note dim">{t('loading')}</div>{/if}
	</td>
	{#if counts}
		<td class="num" style={cell(counts.flags, C.out)}>{counts.flags}</td>
		<td class="num" style={cell(counts.overdue, C.out)}>{counts.overdue}</td>
		<td class="num" style={cell(counts.stale, C.stale)}>{counts.stale}</td>
	{:else}
		<td class="num dim">—</td>
		<td class="num dim">—</td>
		<td class="num dim">—</td>
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
	.note {
		margin: 2px 0 0 30px;
		font-size: 11.5px;
		overflow-wrap: anywhere;
	}
	.note .btn {
		margin-left: 4px;
	}
	/* a phone: the note takes the full width of the name cell, so a scope breaks between words */
	@media (max-width: 600px) {
		td {
			padding: 5px 5px;
		}
		.note {
			margin-left: 0;
		}
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
