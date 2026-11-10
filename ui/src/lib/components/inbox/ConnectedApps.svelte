<script lang="ts">
	// Remote MCP clients you approved over OAuth (claude.ai and others): profiles, scopes, expiry, last
	// use, and revoke. Revoking only narrows access, so it needs no step-up.
	import { listConsents, revokeConsent, type OAuthConsentOut } from '#lib/api/inbox';
	import type { ProfileSummary } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, scopeText, tf } from '#lib/components/pages1/util.svelte';
	import { ago, day, plain } from '#lib/components/pages2/fmt';
	import { t } from '#lib/i18n/index.svelte';
	import { liveConsents } from '#lib/logic/people';

	let { profiles }: { profiles: ProfileSummary[] } = $props();
	const consents = new Loader<OAuthConsentOut[]>();
	const load = () => consents.run(async () => (await listConsents()).items);
	$effect(() => {
		load();
	});
	let busy = $state(false);
	let error = $state<{ id: string; text: string; raw: string } | null>(null);
	let confirming = $state<string | null>(null);

	async function revoke(id: string) {
		busy = true;
		error = null;
		try {
			await revokeConsent(id);
			confirming = null;
		} catch (e) {
			error = { id, text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
			load();
		}
	}
	const live = $derived(liveConsents(consents.data));
	const name = (id: string) => profiles.find((p) => p.id === id)?.display_name ?? id.slice(0, 8);
	const appName = (c: OAuthConsentOut) => plain(c.client_name) || host(c.client_id);
	const lastUse = (c: OAuthConsentOut) =>
		c.last_used_at ?? (c.tokens ?? []).map((x) => x.last_used_at).filter((x): x is string => !!x).sort().at(-1) ?? null;
	const host = (u: string) => {
		try {
			return new URL(u).host;
		} catch {
			return u;
		}
	};
</script>

<section class="card" aria-labelledby="ca-h">
	<h3 id="ca-h">{t('fu_ca')}<span class="r">{t('fu_ca_r')}</span></h3>
	{#if consents.error}
		<NotReady error={consents.error} onretry={load} />
	{:else if consents.loading && !consents.data}
		<div class="empty">{t('loading')}</div>
	{:else}
		<div class="tscroll">
		<table class="t stack ca">
			<tbody>
				{#each live as c (c.id)}
					<tr>
						<td class="who">
							<b>{appName(c)}</b>
							<div class="dim mono url" title={c.client_id}>{c.client_id}</div>
						</td>
						<td>
							<div>{c.profile_ids.map(name).join(', ')}</div>
							<div class="scopes">{#each c.scopes as s (s)}<span class="chip mono" title={scopeText(s)}>{s}</span>{/each}</div>
						</td>
						<td class="muted">
							<div>{t('i2_t_last')}: <span class="num" style="white-space:nowrap">{ago(lastUse(c))}</span></div>
							<div class="dim"><span class="num" style="white-space:nowrap">{tf('fu_ca_until', { date: day(c.expires_at) })}</span></div>
						</td>
						<td class="acts-cell" style="text-align:right;white-space:nowrap">
							<button class="btn sm no" disabled={busy} aria-expanded={confirming === c.id} aria-label="{t('fu_ca_revoke')} {appName(c)}" onclick={() => (confirming = c.id)}>{t('fu_ca_revoke')}</button>
						</td>
					</tr>
					{#if confirming === c.id || error?.id === c.id}
						<tr>
							<td colspan="4">
								{#if confirming === c.id}
									<ConfirmRow
										text={tf('i2_revoke_confirm', { label: appName(c) })}
										confirmLabel={t('fu_ca_revoke_yes')}
										{busy}
										onconfirm={() => revoke(c.id)}
										oncancel={() => (confirming = null)}
									/>
								{/if}
								{#if error?.id === c.id}<div class="p2err" role="alert" title={error.raw}>{tf('i2_revoke_failed', { label: appName(c), msg: error.text })}</div>{/if}
							</td>
						</tr>
					{/if}
				{:else}
					<tr><td colspan="4" style="font-weight:400"><div class="empty">{t('fu_ca_none')}</div></td></tr>
				{/each}
			</tbody>
		</table>
		</div>
	{/if}
	<div class="note" style="margin-top:6px">{t('fu_ca_note')}</div>
</section>

<style>
	.scopes {
		display: flex;
		flex-wrap: wrap;
		gap: 3px;
		margin-top: 3px;
	}
	.who {
		min-width: 0;
		max-width: 220px;
	}
	.url {
		font-size: 10.5px;
		font-weight: 400;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	/* Tablet: a narrower name column keeps Revoke inside the card. */
	@media (max-width: 1100px) {
		.who {
			max-width: 150px;
		}
	}
	@media (max-width: 760px) {
		.who {
			max-width: none;
		}
		/* Name and address, then profiles and scopes on their own line, then the dates and Revoke. */
		table.ca td:nth-child(2) {
			display: block;
		}
	}
</style>
