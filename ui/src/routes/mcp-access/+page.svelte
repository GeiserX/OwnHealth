<script lang="ts">
	// Mockup screen #mcp-access: personal access tokens for MCP clients (create, scope, revoke, last
	// used), the Claude Code connection snippet, the append-only audit log, egress budget, the tools.
	import '#lib/components/pages2/pages2.css';
	import { getTokenUsage, listAudit, listTokens, revokeToken } from '#lib/api';
	import { listConsents, type OAuthConsentOut } from '#lib/api/inbox';
	import type { AuditOut, TokenOut, TokenUsage } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, scopeText, tf, tp } from '#lib/components/pages1/util.svelte';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import TokenForm from '#lib/components/pages2/TokenForm.svelte';
	import { ago, day, int, plain, stamp } from '#lib/components/pages2/fmt';
	import { fmtNum, t } from '#lib/i18n/index.svelte';
	import { copyText } from '#lib/logic/inbox';
	import { auditActor, auditOut, auditResult, auditRows, auditWhat, liveConsents } from '#lib/logic/people';
	import { app } from '#lib/state/app.svelte';

	// Each tool and the scopes it needs (docs/design/mcp.md 2.3). `any` lists alternatives, `note` adds
	// a dict key or a daily cap after them.
	const TOOLS: { name: string; scopes: string[]; any?: boolean; note?: [string, Record<string, string | number>?] }[] = [
		{ name: 'get_health_status', scopes: ['read:summary'] },
		{ name: 'search_observations', scopes: ['read:labs', 'read:vitals', 'read:wearables'], any: true },
		{ name: 'get_trend', scopes: ['read:labs', 'read:vitals', 'read:wearables'], any: true },
		{ name: 'get_genome_findings', scopes: ['read:genome'] },
		{ name: 'lookup_variant', scopes: ['read:genome_raw'], note: ['i2_tool_perday', { n: 200 }] },
		{ name: 'check_drug', scopes: ['read:genome', 'read:records', 'read:labs'], any: true },
		{ name: 'get_source', scopes: [], note: ['i2_tool_cited'] },
		{ name: 'propose', scopes: ['write:propose'], note: ['i2_tool_queue'] },
		{ name: 'list_pending_documents', scopes: ['write:propose'] },
		{ name: 'get_document_page', scopes: ['read:documents_text', 'write:propose'], note: ['i2_tool_perday', { n: 300 }] }
	];
	const origin = typeof location !== 'undefined' ? location.origin : '';
	const endpoint = `${origin}/mcp`;

	const tokens = new Loader<TokenOut[]>();
	const audit = new Loader<AuditOut[]>();
	const usage = new Loader<TokenUsage>();
	const consents = new Loader<OAuthConsentOut[]>();
	let auditProfile = $state('');
	let auditMode = $state<'agents' | 'all'>('agents');
	let creating = $state(false);
	let fresh = $state<{ token: TokenOut; plaintext: string } | null>(null);
	// The last copy: which box, and whether the clipboard took it (false: the text is selected instead).
	let copied = $state<{ what: string; ok: boolean } | null>(null);
	let secretEl: HTMLElement | undefined = $state();
	let snippetEl: HTMLElement | undefined = $state();
	let confirming = $state<string | null>(null);
	let busy = $state(false);
	let revokeError = $state<{ id: string; text: string; raw: string } | null>(null);

	const loadTokens = () => tokens.run(async () => (await listTokens()).items);
	// 'Agents (MCP)' asks the server for MCP rows only, so 60 page views never hide what an agent did.
	const loadAudit = () =>
		audit.run(async () => (await listAudit({ limit: 60, profile: auditProfile || undefined, via: auditMode === 'agents' ? 'mcp' : undefined })).items);
	const loadConsents = () => consents.run(async () => (await listConsents()).items);
	$effect(() => {
		loadTokens();
		loadConsents();
	});
	$effect(() => {
		void [auditProfile, auditMode];
		loadAudit();
	});
	const active = $derived((tokens.data ?? []).filter((x) => !x.revoked_at));
	const apps = $derived(liveConsents(consents.data));
	const appNames = $derived((consents.data ?? []).map((c) => ({ client_id: c.client_id, name: plain(c.client_name) })));
	const budgetToken = $derived(active.find((x) => x.last_used_at) ?? active[0] ?? null);
	$effect(() => {
		const id = budgetToken?.id;
		if (id) usage.run(() => getTokenUsage(id));
	});
	const shownAudit = $derived(auditRows(audit.data, auditMode));

	const snippet = $derived(`claude mcp add --transport http ownhealth ${endpoint} --header "Authorization: Bearer ${fresh?.plaintext ?? 'oh_pat_…'}"`);
	async function copy(text: string, what: string, box: HTMLElement | undefined) {
		copied = { what, ok: await copyText(typeof navigator === 'undefined' ? null : navigator.clipboard, text, box) };
	}
	async function revoke(x: TokenOut) {
		busy = true;
		revokeError = null;
		try {
			await revokeToken(x.id);
			confirming = null;
			if (fresh?.token.id === x.id) fresh = null;
			loadTokens();
			loadAudit();
		} catch (e) {
			revokeError = { id: x.id, text: tf('i2_revoke_failed', { label: x.label, msg: errText(e) }), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}
	const name = (slug: string) => app.me?.profiles.find((p) => p.slug === slug)?.display_name.split(' ')[0] ?? slug;
	const tone = (s: string) => (s.startsWith('write') ? 'w' : s.includes('raw') || s.includes('sensitive') ? 'r' : '');
	function resources(a: AuditOut): string {
		if (a.args_summary && !a.args_summary.startsWith('h:')) return a.args_summary;
		const n = a.resource_ids?.length ?? 0;
		return [a.resource_type, n ? `×${n}` : null, a.src_ids?.length ? `src ×${a.src_ids.length}` : null].filter(Boolean).join(' · ') || '—';
	}
	const pct = (n: number, of: number | undefined) => (of ? Math.min(100, (n / of) * 100) : 0);
	const mb = (bytes: number, digits: number) => fmtNum(bytes / 1e6, digits);
</script>

{#snippet copyHand(what: string)}
	{#if copied?.what === what && !copied.ok}<div class="note" role="alert" style="margin-top:6px;color:var(--watch)">{t('fu_copy_hand')}</div>{/if}
{/snippet}

<div class="ph">
	<div>
		<h1>{t('nav_mcp')}</h1>
		<div class="sub">{t('i2_mcp_sub1')} <span class="mono">/mcp</span> {t('i2_mcp_sub2')}</div>
	</div>
	<div class="right">
		<PhaseChip phase={t('i2_mcp_ph1')} /><PhaseChip phase={t('i2_mcp_ph2')} />
		<a class="btn" href="/settings#ca-h">{t('fu_ca')}</a>
		<button class="btn pri" onclick={() => (creating = !creating)} aria-expanded={creating}>{t('i2_tok_new')}</button>
	</div>
</div>

<div class="mcp">
	<div class="grid" style="align-content:start">
		{#if creating}
			<TokenForm
				profiles={app.me?.profiles ?? []}
				oncancel={() => (creating = false)}
				oncreated={(token, plaintext) => {
					fresh = { token, plaintext };
					creating = false;
					copied = null;
					loadTokens();
					loadAudit();
				}}
			/>
		{/if}
		{#if fresh}
			<section class="card" style="border-color:rgba(224,168,74,.55)" aria-live="polite">
				<h3>{tf('i2_tok_made', { label: fresh.token.label })}<span class="r"><button class="btn sm" onclick={() => fresh && copy(fresh.plaintext, 'token', secretEl)}>{copied?.what === 'token' && copied.ok ? t('i2_copied') : t('i2_copy')}</button></span></h3>
				<div class="p2secret" bind:this={secretEl}>{fresh.plaintext}</div>
				{@render copyHand('token')}
				<div class="note" style="margin-top:6px;color:var(--watch)">{t('i2_tok_once')}</div>
				<div class="p2row" style="margin-top:6px"><button class="btn sm" onclick={() => (fresh = null)}>{t('i2_tok_saved')}</button></div>
			</section>
		{/if}
		<div class="card" style="padding:6px 6px 2px">
			{#if tokens.error}
				<NotReady error={tokens.error} onretry={loadTokens} />
			{:else}
				<div class="tscroll">
					<table class="t stack">
						<thead><tr><th>{t('i2_t_token')}</th><th>{t('profile')}</th><th>{t('i2_t_scopes')}</th><th style="white-space:nowrap">{t('i2_t_last')}</th><th>{t('i2_t_exp')}</th><th></th></tr></thead>
						<tbody>
							{#each active as x (x.id)}
								<tr>
									<td><b>{x.label}</b><div class="dim mono" style="font-size:10.5px">{x.prefix}…</div></td>
									<td>{x.profiles.map(name).join(', ')}</td>
									<td>{#each x.scopes as s (s)}<span class="chip mono {tone(s)}" title={scopeText(s)}>{s}</span> {/each}</td>
									<td><span class="num" style="white-space:nowrap">{ago(x.last_used_at)}</span></td>
									<td><span class="num" style="white-space:nowrap">{x.expires_at ? day(x.expires_at) : t('i2_never')}</span></td>
									<td class="acts-cell" style="text-align:right">
										<button class="btn sm no" disabled={busy} aria-expanded={confirming === x.id} onclick={() => (confirming = x.id)} aria-label="{t('i2_revoke')} {x.label}">{t('i2_revoke')}</button>
									</td>
								</tr>
								{#if confirming === x.id || revokeError?.id === x.id}
									<tr>
										<td colspan="6">
											{#if confirming === x.id}
												<ConfirmRow text={tf('i2_revoke_confirm', { label: x.label })} confirmLabel={t('fu_ca_revoke_yes')} {busy} onconfirm={() => revoke(x)} oncancel={() => (confirming = null)} />
											{/if}
											{#if revokeError?.id === x.id}<div class="p2err" role="alert" title={revokeError.raw}>{revokeError.text}</div>{/if}
										</td>
									</tr>
								{/if}
							{:else}
								<tr>
									<td colspan="6">
										<!-- "No agent can read anything" only once both lists say so: an OAuth app may still have access. -->
										{#if tokens.loading || !tokens.data || (!consents.data && !consents.error)}
											<div class="empty">{t('loading')}</div>
										{:else if apps.length}
											<div class="empty">{tp('i2_oauth_live', apps.length)} <a href="/settings#ca-h">{t('fu_ca')}</a></div>
										{:else if consents.error}
											<NotReady error={consents.error} onretry={loadConsents} />
										{:else}
											<div class="empty">{t('i2_tok_none')}</div>
										{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
				{#if active.length && apps.length}
					<div class="note" style="padding:4px 8px 6px">{tp('i2_oauth_live', apps.length)} <a href="/settings#ca-h">{t('fu_ca')}</a></div>
				{/if}
			{/if}
		</div>
		<section class="card">
			<h3>{t('i2_add_cc')}<span class="r"><button class="btn sm" onclick={() => copy(snippet, 'snip', snippetEl)}>{copied?.what === 'snip' && copied.ok ? t('i2_copied') : t('i2_copy')}</button></span></h3>
			<pre class="code mono" bind:this={snippetEl}>{snippet}</pre>
			{@render copyHand('snip')}
			<div class="note" style="margin-top:6px">{t('i2_cc_note')}</div>
		</section>
		<section class="card" style="padding:8px 6px 2px">
			<h3 style="padding:0 8px">
				{t('i2_audit')}
				<span class="r p2row">
					{t('i2_audit_d')}
					<select class="p2in" bind:value={auditMode} aria-label={t('i2_audit_show')}>
						<option value="agents">{t('i2_audit_agents')}</option>
						<option value="all">{t('i2_audit_all')}</option>
					</select>
					<select class="p2in" bind:value={auditProfile} aria-label={t('profile')}>
						<option value="">{t('i2_all_profiles')}</option>
						{#each app.me?.profiles ?? [] as p (p.id)}<option value={p.slug}>{p.display_name}</option>{/each}
					</select>
				</span>
			</h3>
			{#if audit.error}
				<NotReady error={audit.error} onretry={loadAudit} />
			{:else}
				<div class="tscroll">
					<table class="t nw">
						<thead><tr><th>{t('i2_a_time')}</th><th>{t('i2_a_who')}</th><th>{t('i2_a_tool')}</th><th>{t('profile')}</th><th>{t('i2_a_result')}</th><th style="text-align:right">{t('i2_a_out')}</th><th>{t('i2_a_res')}</th></tr></thead>
						<tbody>
							{#each shownAudit as a (a.id)}
								{@const who = auditActor(a.actor, appNames)}
								{@const res = auditResult(a.result_kind)}
								{@const out = auditOut(a)}
								<tr>
									<td class="num muted" title={a.ts}>{stamp(a.ts)}</td>
									<td><div class="cl" style="max-width:160px" title={who.raw}>{who.label}{a.via === 'mcp' ? '' : ` · ${a.via}`}</div></td>
									<td class="mono"><div class="cl" style="max-width:170px" title={a.tool ?? a.action}>{auditWhat(a)}</div></td>
									<td>{a.profile ?? '—'}</td>
									<!-- Result sits before the free-width columns so a denied call is never pushed off the card. -->
									<td><span class="pill {res.tone}" title={a.result_kind ?? 'ok'}><i aria-hidden="true"></i>{t(res.key) === res.key ? (a.result_kind ?? 'ok') : t(res.key)}</span></td>
									<td class="num" style="text-align:right">{'kb' in out ? tf('i2_kb', { n: fmtNum(out.kb, 1) }) : int(out.rows)}</td>
									<td class="muted"><div class="cl" style="max-width:200px" title={resources(a)}>{resources(a)}</div></td>
								</tr>
							{:else}
								<tr><td colspan="7"><div class="empty">{audit.loading || !audit.data ? t('loading') : auditMode === 'agents' ? t('i2_audit_none_agents') : t('i2_audit_none')}</div></td></tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</section>
	</div>

	<div class="grid" style="align-content:start">
		<section class="card">
			<h3>{t('i2_egress')}<span class="r">{budgetToken?.label ?? ''}</span></h3>
			{#if tokens.error}
				<NotReady error={tokens.error} onretry={loadTokens} />
			{:else if !tokens.data}
				<div class="empty">{t('loading')}</div>
			{:else if !budgetToken && apps.length}
				<div class="muted" style="font-size:12px">{tp('i2_oauth_live', apps.length)} <a href="/settings#ca-h">{t('fu_ca')}</a></div>
			{:else if !budgetToken && consents.data}
				<div class="muted" style="font-size:12px">{t('i2_egress_none')}</div>
			{:else if !budgetToken}
				{#if consents.error}<NotReady error={consents.error} onretry={loadConsents} />{:else}<div class="empty">{t('loading')}</div>{/if}
			{:else if usage.error}
				<NotReady error={usage.error} />
			{:else if usage.data}
				{@const u = usage.data.today}
				{@const lim = usage.data.limits}
				<div style="display:flex;align-items:baseline;gap:6px;flex-wrap:wrap">
					<span style="font-size:22px;font-weight:650" class="num">{int(u.facts)}</span>
					<span class="muted">{tf('i2_egress_facts', { max: int(lim.egress_facts_per_day), mb: mb(u.bytes, 1), maxmb: mb(lim.egress_bytes_per_day ?? 0, 0) })}</span>
				</div>
				<div class="meter" style="margin:6px 0 10px" aria-hidden="true"><i style="width:{pct(u.facts, lim.egress_facts_per_day)}%;background:var(--accent)"></i></div>
				<div style="display:flex;align-items:baseline;gap:6px;flex-wrap:wrap">
					<span style="font-size:15px;font-weight:650" class="num">{int(u.lookups)}</span>
					<span class="muted">{tf('i2_egress_lookups', { max: int(lim.genome_raw_lookups_per_day) })}</span>
				</div>
				<div class="meter" style="margin:6px 0 8px" aria-hidden="true"><i style="width:{pct(u.lookups, lim.genome_raw_lookups_per_day)}%;background:var(--watch)"></i></div>
			{:else}
				<div class="empty">{t('loading')}</div>
			{/if}
			<div class="note">{t('i2_egress_note')}</div>
		</section>
		<section class="card">
			<h3>{t('i2_tools')}<span class="r">{t('i2_tools_d')}</span></h3>
			{#each TOOLS as x, i (x.name)}
				<div class="tool">
					<span class="dim num" style="width:14px">{i + 1}</span><code class="mono">{x.name}</code>
					<span class="dim need">
						{#if x.any}{t('i2_tool_anyof') + ' '}{/if}{#each x.scopes as s, k (s)}{k ? ' · ' : ''}<span title={scopeText(s)}>{s}</span>{/each}{#if x.note}{(x.scopes.length ? ' · ' : '') + tf(x.note[0], x.note[1] ?? {})}{/if}
					</span>
				</div>
			{/each}
			<div class="note" style="margin-top:6px">{t('i2_tools_note')}</div>
		</section>
		<section class="card">
			<h3>{t('i2_server')}</h3>
			<table class="t" style="font-size:12px">
				<tbody>
					<tr><td class="dim">{t('i2_endpoint')}</td><td class="mono">{endpoint}</td></tr>
					<tr><td class="dim">{t('i2_transport')}</td><td>Streamable HTTP · 2026-07-28 (2025-11-25 {t('i2_fallback')})</td></tr>
					<tr><td class="dim">{t('i2_freetext')}</td><td>{t('i2_marked')} <span class="mono">trust: untrusted_text</span></td></tr>
					<tr><td class="dim">{t('i2_doctext')}</td><td><span class="mono">read:documents_text</span> · {t('i2_off_default')}</td></tr>
				</tbody>
			</table>
		</section>
	</div>
</div>

<StepUpDialog />

<style>
	.chip.w {
		color: var(--watch);
		border-color: rgba(224, 168, 74, 0.5);
	}
	.chip.r {
		color: var(--out);
		border-color: rgba(239, 107, 115, 0.5);
	}
	td .chip {
		margin: 1px 0;
	}
	/* A long snippet scrolls inside its card. */
	pre.code {
		overflow-x: auto;
	}
	.need {
		margin-left: auto;
		font-size: 11px;
		text-align: right;
	}
	.cl {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
