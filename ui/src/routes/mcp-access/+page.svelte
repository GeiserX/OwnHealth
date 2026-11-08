<script lang="ts">
	// Mockup screen #mcp-access: personal access tokens for MCP clients (create, scope, revoke, last
	// used), the Claude Code connection snippet, the append-only audit log, egress budget, the 8 tools.
	import '#lib/components/pages2/pages2.css';
	import { getTokenUsage, listAudit, listTokens, revokeToken } from '#lib/api';
	import type { AuditOut, TokenOut, TokenUsage } from '#lib/api/types';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import TokenForm from '#lib/components/pages2/TokenForm.svelte';
	import { ago, day, int, stamp } from '#lib/components/pages2/fmt';
	import { t } from '#lib/i18n/index.svelte';
	import { app } from '#lib/state/app.svelte';

	const TOOLS: [string, string][] = [
		['get_health_status', 'read:summary'],
		['search_observations', 'read:labs · read:vitals · read:wearables'],
		['get_trend', 'read:labs · read:vitals · read:wearables'],
		['get_genome_findings', 'read:genome'],
		['lookup_variant', 'read:genome_raw · 200/day'],
		['check_drug', 'any of read:genome · read:records · read:labs'],
		['get_source', 'scope of the cited fact (+ read:documents_text)'],
		['propose', 'write:propose → review queue']
	];
	const origin = typeof location !== 'undefined' ? location.origin : '';
	const endpoint = `${origin}/mcp`;

	const tokens = new Loader<TokenOut[]>();
	const audit = new Loader<AuditOut[]>();
	const usage = new Loader<TokenUsage>();
	let auditProfile = $state('');
	let creating = $state(false);
	let fresh = $state<{ token: TokenOut; plaintext: string } | null>(null);
	let copied = $state('');
	let actionError = $state<string | null>(null);

	const loadTokens = () => tokens.run(async () => (await listTokens()).items);
	const loadAudit = () => audit.run(async () => (await listAudit({ limit: 60, profile: auditProfile || undefined })).items);
	$effect(() => {
		loadTokens();
	});
	$effect(() => {
		void auditProfile;
		loadAudit();
	});
	const active = $derived((tokens.data ?? []).filter((x) => !x.revoked_at));
	const budgetToken = $derived(active.find((x) => x.last_used_at) ?? active[0] ?? null);
	$effect(() => {
		const id = budgetToken?.id;
		if (id) usage.run(() => getTokenUsage(id));
	});

	const snippet = $derived(`claude mcp add --transport http ownhealth ${endpoint} --header "Authorization: Bearer ${fresh?.plaintext ?? 'oh_pat_…'}"`);
	async function copy(text: string, what: string) {
		try {
			await navigator.clipboard.writeText(text);
			copied = what;
		} catch {
			copied = '';
		}
	}
	async function revoke(x: TokenOut) {
		if (!confirm(tf('i2_revoke_confirm', { label: x.label }))) return;
		actionError = null;
		try {
			await revokeToken(x.id);
			if (fresh?.token.id === x.id) fresh = null;
			loadTokens();
			loadAudit();
		} catch (e) {
			actionError = e instanceof Error ? e.message : String(e);
		}
	}
	const name = (slug: string) => app.me?.profiles.find((p) => p.slug === slug)?.display_name.split(' ')[0] ?? slug;
	const tone = (s: string) => (s.startsWith('write') ? 'w' : s.includes('raw') || s.includes('sensitive') ? 'r' : '');
	function resources(a: AuditOut): string {
		if (a.args_summary && !a.args_summary.startsWith('h:')) return a.args_summary;
		const n = a.resource_ids?.length ?? 0;
		return [a.resource_type, n ? `×${n}` : null, a.src_ids?.length ? `src ×${a.src_ids.length}` : null].filter(Boolean).join(' · ') || '—';
	}
	// MCP rows carry the tool name; UI and API rows carry the route, shortened to its tail.
	const toolName = (a: AuditOut) => (a.tool ?? a.action).replace(/^(GET|POST|PUT|DELETE) \/api\/(profiles\/\{p\}\/)?/, '');
	const resultTone = (r: string | null) => (!r || r === 'ok' ? 'p-ok' : r === 'queued' ? 'p-info' : 'p-out');
	const pct = (n: number, of: number | undefined) => (of ? Math.min(100, (n / of) * 100) : 0);
</script>

<div class="ph">
	<div>
		<h1>{t('nav_mcp')}</h1>
		<div class="sub">{t('i2_mcp_sub1')} <span class="mono">/mcp</span> {t('i2_mcp_sub2')}</div>
	</div>
	<div class="right">
		<PhaseChip phase={t('i2_mcp_ph1')} /><PhaseChip phase={t('i2_mcp_ph2')} />
		<a class="btn" href="/settings">{t('i2_settings')}</a>
		<button class="btn pri" onclick={() => (creating = !creating)} aria-expanded={creating}>{t('i2_tok_new')}</button>
	</div>
</div>

{#if actionError}<div class="p2err" role="alert" style="margin-bottom:8px">{actionError}</div>{/if}

<div class="mcp">
	<div class="grid" style="align-content:start">
		{#if creating}
			<TokenForm
				profiles={app.me?.profiles ?? []}
				oncancel={() => (creating = false)}
				oncreated={(token, plaintext) => {
					fresh = { token, plaintext };
					creating = false;
					copied = '';
					loadTokens();
					loadAudit();
				}}
			/>
		{/if}
		{#if fresh}
			<section class="card" style="border-color:rgba(224,168,74,.55)" aria-live="polite">
				<h3>{tf('i2_tok_made', { label: fresh.token.label })}<span class="r"><button class="btn sm" onclick={() => fresh && copy(fresh.plaintext, 'token')}>{copied === 'token' ? t('i2_copied') : t('i2_copy')}</button></span></h3>
				<div class="p2secret">{fresh.plaintext}</div>
				<div class="note" style="margin-top:6px;color:var(--watch)">{t('i2_tok_once')}</div>
				<div class="p2row" style="margin-top:6px"><button class="btn sm" onclick={() => (fresh = null)}>{t('i2_tok_saved')}</button></div>
			</section>
		{/if}
		<div class="card" style="padding:6px 6px 2px;overflow-x:auto">
			{#if tokens.error}
				<NotReady error={tokens.error} onretry={loadTokens} />
			{:else}
				<table class="t">
					<thead><tr><th>{t('i2_t_token')}</th><th>{t('profile')}</th><th>{t('i2_t_scopes')}</th><th style="white-space:nowrap">{t('i2_t_last')}</th><th>{t('i2_t_exp')}</th><th></th></tr></thead>
					<tbody>
						{#each active as x (x.id)}
							<tr>
								<td style="white-space:nowrap"><b>{x.label}</b><div class="dim mono" style="font-size:10.5px">{x.prefix}…</div></td>
								<td>{x.profiles.map(name).join(', ')}</td>
								<td>{#each x.scopes as s (s)}<span class="chip mono {tone(s)}">{s}</span> {/each}</td>
								<td style="white-space:nowrap">{ago(x.last_used_at)}</td>
								<td style="white-space:nowrap">{x.expires_at ? day(x.expires_at) : t('i2_never')}</td>
								<td><button class="btn sm no" onclick={() => revoke(x)} aria-label="{t('i2_revoke')} {x.label}">{t('i2_revoke')}</button></td>
							</tr>
						{:else}
							<tr><td colspan="6"><div class="empty">{tokens.loading ? t('loading') : t('i2_tok_none')}</div></td></tr>
						{/each}
					</tbody>
				</table>
			{/if}
		</div>
		<section class="card">
			<h3>{t('i2_add_cc')}<span class="r"><button class="btn sm" onclick={() => copy(snippet, 'snip')}>{copied === 'snip' ? t('i2_copied') : t('i2_copy')}</button></span></h3>
			<pre class="code mono">{snippet}</pre>
			<div class="note" style="margin-top:6px">{t('i2_cc_note')}</div>
		</section>
		<section class="card" style="padding:8px 6px 2px;overflow-x:auto">
			<h3 style="padding:0 8px">
				{t('i2_audit')}
				<span class="r p2row">
					{t('i2_audit_d')}
					<select class="p2in" bind:value={auditProfile} aria-label={t('profile')}>
						<option value="">{t('i2_all_profiles')}</option>
						{#each app.me?.profiles ?? [] as p (p.id)}<option value={p.slug}>{p.display_name}</option>{/each}
					</select>
				</span>
			</h3>
			{#if audit.error}
				<NotReady error={audit.error} onretry={loadAudit} />
			{:else}
				<div style="overflow-x:auto">
				<table class="t nw">
					<thead><tr><th>{t('i2_a_time')}</th><th>{t('i2_a_who')}</th><th>{t('i2_a_tool')}</th><th>{t('profile')}</th><th>{t('i2_a_result')}</th><th style="text-align:right">{t('i2_a_out')}</th><th>{t('i2_a_res')}</th></tr></thead>
					<tbody>
						{#each audit.data ?? [] as a (a.id)}
							<tr>
								<td class="num muted" title={a.ts}>{stamp(a.ts)}</td>
								<td><div class="cl" style="max-width:160px" title={a.actor}>{a.actor}{a.via === 'mcp' ? '' : ` · ${a.via}`}</div></td>
								<td class="mono"><div class="cl" style="max-width:170px" title={a.tool ?? a.action}>{toolName(a)}</div></td>
								<td>{a.profile ?? '—'}</td>
								<!-- Result sits before the free-width columns so a denied call is never pushed off the card. -->
								<td><span class="pill {resultTone(a.result_kind)}"><i aria-hidden="true"></i>{a.result_kind ?? 'ok'}</span></td>
								<td class="num" style="text-align:right">{int(a.rows_out)}</td>
								<td class="muted"><div class="cl" style="max-width:200px" title={resources(a)}>{resources(a)}</div></td>
							</tr>
						{:else}
							<tr><td colspan="7"><div class="empty">{audit.loading ? t('loading') : t('i2_audit_none')}</div></td></tr>
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
			{#if !budgetToken}
				<div class="muted" style="font-size:12px">{t('i2_egress_none')}</div>
			{:else if usage.error}
				<NotReady error={usage.error} />
			{:else if usage.data}
				{@const u = usage.data.today}
				{@const lim = usage.data.limits}
				<div style="display:flex;align-items:baseline;gap:6px">
					<span style="font-size:22px;font-weight:650" class="num">{int(u.facts)}</span>
					<span class="muted">{tf('i2_egress_facts', { max: int(lim.egress_facts_per_day), mb: (u.bytes / 1e6).toFixed(1), maxmb: ((lim.egress_bytes_per_day ?? 0) / 1e6).toFixed(0) })}</span>
				</div>
				<div class="meter" style="margin:6px 0 10px" aria-hidden="true"><i style="width:{pct(u.facts, lim.egress_facts_per_day)}%;background:var(--accent)"></i></div>
				<div style="display:flex;align-items:baseline;gap:6px">
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
			{#each TOOLS as x, i (x[0])}
				<div class="tool"><span class="dim num" style="width:14px">{i + 1}</span><code class="mono">{x[0]}</code><span class="dim" style="margin-left:auto;font-size:11px;text-align:right">{x[1]}</span></div>
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
	.cl {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
