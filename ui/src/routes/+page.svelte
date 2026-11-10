<script lang="ts">
	// Health at a glance (mockup screen #dashboard). Everything shown comes from GET /profiles/{p}/dashboard
	// (lane E): status words, markers, sparklines, changes, due items. The page never computes a status.
	import { getDashboard, listReview } from '#lib/api';
	import type { Dashboard, ReviewItem, Status, SystemCard } from '#lib/api/types';
	import { C } from '#lib/components/colors';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, shortHash, tf, tp, unitLabel } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import Sparkline from '#lib/components/Sparkline.svelte';
	import StatusChip from '#lib/components/StatusChip.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';

	// The rule pack ships inside the package. The link names only the system's pack file, never the rule that fired.
	const RULES_URL = 'https://github.com/GeiserX/OwnHealth/blob/main/backend/src/ownhealth/rules/pack/';
	const dash = new Loader<Dashboard>();
	const queue = new Loader<{ items: ReviewItem[]; open: number }>();
	const profile = $derived(currentProfile());

	function load() {
		const slug = profile?.slug;
		const lang = i18n.lang;
		if (!slug) return;
		dash.run(() => getDashboard(slug, lang), slug);
		queue.run(() => listReview({ status: 'open', profile: slug }), slug);
	}
	$effect(load);

	// "rows from 3 documents, 1 proposed by an agent", counted from the open queue itself.
	const queueLine = $derived.by(() => {
		const items = queue.data?.items ?? [];
		const docs = new Set(items.map((i) => i.document_id).filter(Boolean)).size;
		const agent = items.filter((i) => i.kind === 'agent_proposal').length;
		return tf(docs === 1 ? 'h1_needs_d1' : 'h1_needs_d', { d: docs, a: agent });
	});
	const reviewOpen = $derived(queue.data?.open ?? dash.data?.review_open ?? 0);
	// One source chip on the right when every medication comes from the same record, as in the mockup.
	const medSrc = $derived.by(() => {
		const meds = dash.data?.medications ?? [];
		return meds.length && meds.every((m) => m.src === meds[0].src) ? meds[0].src : null;
	});
	// The scope a withheld card needs: the backend's `needs` when sent, else by the card's source.
	const needs = (s: SystemCard) => s.needs ?? (s.key === 'sleep' || s.key === 'fitness' ? 'read:wearables' : 'read:labs');
	const statusColor = (s: Status) => (s === 'ok' ? 'inherit' : C[s]);
	const DUE_ICON: Record<string, string> = {
		lab: '<path d="M9 3h6M10 3v6l-5 9a2 2 0 0 0 2 3h10a2 2 0 0 0 2-3l-5-9V3"/>',
		vaccine: '<path d="m18 2 4 4M17 7l-9 9M14 4l6 6M5 19l-3 3M7 13l4 4"/>',
		screening: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>'
	};
	const fmtTime = (iso: string) => iso.slice(0, 16).replace('T', ' ') + ' UTC';
</script>

<div class="ph">
	<div>
		<h1>{t('dash_t')}{profile ? ' · ' + profile.display_name : ''}</h1>
		{#if dash.data?.subtitle}<div class="sub">{dash.data.subtitle}</div>{/if}
	</div>
	<div class="right">
		<PhaseChip phase="v0.1" />
		<button class="btn" aria-disabled="true" title={t('h1_soon_btn')}>{t('ice')} <PhaseChip phase="v1.0" /></button>
		<button class="btn" aria-disabled="true" title={t('h1_soon_btn')}>{t('prep')} <PhaseChip phase="v0.3" /></button>
	</div>
</div>

{#if dash.loading && !dash.data}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if dash.error}
	<div class="card"><NotReady error={dash.error} onretry={load} /></div>
	<div class="card" style="margin-top:12px"><div class="empty">{t('h1_nodash')}</div></div>
{:else if dash.data}
	{@const d = dash.data}
	<div class="meds">
		<b style="font-size:12px;color:var(--muted);white-space:nowrap">{t('meds')}</b>
		{#each d.medications as m (m.src + m.name)}
			<span class="med" title={m.since ? `${m.name} · ${m.since}` : m.name}>
				<b>{m.name}</b>{#if m.strength}&nbsp;{m.strength}{/if}
				{#if m.pgx}
					<span class="pgx" style="color:var(--watch);border-color:rgba(224,168,74,.5)" title="{m.pgx.gene} · {m.pgx.phenotype}{m.pgx.level ? ' · CPIC ' + m.pgx.level : ''} · {m.pgx.text}">{m.pgx.label}</span>
				{/if}
				{#if !medSrc}<SourceChip label="" src={m.src} onopen={openSource} />{/if}
			</span>
		{:else}
			<span class="dim">{t('h1_nomeds')}</span>
		{/each}
		<span style="margin-left:auto">
			{#if medSrc}<SourceChip label={d.medications[0].since ? `Rx · ${d.medications[0].since}` : 'Rx'} src={medSrc} onopen={openSource} />{/if}
		</span>
		<a class="btn sm" href="/drug-check">{t('chk')}</a><PhaseChip phase="v0.1" />
	</div>

	<div class="dash">
		<div class="sys">
			{#each d.systems as s (s.key)}
				<div class="sc">
					<div class="hd">
						<b>{s.name}</b>
						<!-- A card the access does not cover is hidden, not empty: no status word. -->
						{#if s.withheld}<span class="chip" title={tf('h1_withheld_sys', { scope: needs(s) })}>🔒 {t('h1_locked')}</span>{:else}<StatusChip status={s.status} />{/if}
					</div>
					{#if s.withheld}
						<div class="dim" style="font-size:12px;padding:8px 0">{tf('h1_withheld_sys', { scope: needs(s) })}</div>
					{:else}
					{#each s.markers as m (m.alias_group)}
						<div class="mk">
							<div class="n">
								<a href="/biomarkers?code={encodeURIComponent(m.alias_group)}" style="color:inherit">{m.name}</a>{#if m.stale}<span class="stale-b">{t('staleb')}</span>{/if}
							</div>
							<div class="v num" style="color:{statusColor(m.status)}">
								{m.value}<small>{unitLabel(m.unit)}</small><span class="sr-only"> · {t(m.status)}</span>
							</div>
							<div class="s"><SourceChip label={m.src_label ?? m.src ?? ''} src={m.src} onopen={openSource} /></div>
							<div class="g">{#if m.spark.length}<Sparkline values={m.spark} status={m.status} band={m.band} w={64} h={18} />{/if}</div>
						</div>
					{:else}
						<div class="empty" style="padding:8px 0">{t('h1_nomark')}</div>
					{/each}
					{/if}
					<div class="ft">
						<span>{t('newest')} {s.newest ?? '—'}</span>
						<a class="mono" style="font-size:10px;color:inherit" href="{RULES_URL}{s.rule_file ?? s.rule}" target="_blank" rel="noopener" title={t('h1_rule_t')}>{s.rule}</a>
					</div>
				</div>
			{/each}
		</div>

		<div class="grid" style="align-content:start">
			<div class="card">
				<h3>{t('needs')}</h3>
				<div class="review">
					<div class="big num">{reviewOpen}</div>
					<div class="muted" style="font-size:12px">{reviewOpen ? queueLine : t('h1_needs_none')}</div>
					<a class="btn sm" style="margin-left:auto" href="/import-review">{t('open')}</a>
				</div>
			</div>

			<div class="card">
				<h3>{t('changed')}{#if d.changes_vs}<span class="r">{tf('h1_vs', { d: d.changes_vs })}</span>{/if}</h3>
				{#if d.changes.length}
					<div class="chg">
						{#each d.changes as c (c.name)}
							<span>{c.name}</span>
							<span class="num" style="color:{C[c.status]}">{c.from} → {c.to} <small class="muted">{unitLabel(c.unit)}</small></span>
							<span style="color:{C[c.status]}" title={t(c.status)}>{c.direction === 'down' ? '▼' : '▲'}<span class="sr-only">{t(c.status)}</span></span>
						{/each}
					</div>
				{:else}
					<div class="dim" style="font-size:12px">{t('h1_nochg')}</div>
				{/if}
				{#if d.not_measured_this_time.length}
					<div class="note" style="margin-top:8px">{t('h1_notm')} {d.not_measured_this_time.join(', ')}.</div>
				{/if}
			</div>

			<div class="card">
				<h3>{t('due')}<span class="r">{t('duescr')}</span></h3>
				{#each d.due as x (x.title)}
					<div class="due">
						<div class="ic"><svg viewBox="0 0 24 24" aria-hidden="true">{@html DUE_ICON[x.kind] ?? DUE_ICON.lab}</svg></div>
						<div class="w"><div>{x.title}</div><div>{x.why}</div></div>
						{#if x.phase}<PhaseChip phase={x.phase} />{/if}
						<i style="width:8px;height:8px;border-radius:50%;background:{C[x.status]};flex:none" title={t(x.status)}></i><span class="sr-only">{t(x.status)}</span>
					</div>
				{:else}
					<div class="dim" style="font-size:12px">{t('h1_nodue')}</div>
				{/each}
			</div>

			<div class="card">
				<h3>
					{t('gen_h')}
					{#if d.genome?.source}<span class="r"><a href="https://github.com/GeiserX/OwnGenome" target="_blank" rel="noopener">{d.genome.source}</a></span>{/if}
				</h3>
				{#if d.genome}
					<div class="muted" style="font-size:12px">{d.genome.summary}</div>
					<div style="display:flex;gap:6px;margin-top:7px;flex-wrap:wrap">
						{#each d.genome.chips as c (c)}<span class="chip">{c}</span>{/each}
						{#if d.genome.hidden}<span class="chip">🔒 {tp('h1_hidden', d.genome.hidden)}</span>{/if}
					</div>
				{:else}
					<div class="dim" style="font-size:12px">{t('h1_nogen')}</div>
				{/if}
			</div>
		</div>
	</div>

	<div class="foot-hash">
		<span>state_hash <span class="mono" title={d.state_hash}>{shortHash(d.state_hash)}</span></span>
		<span>{t('footer')} <span class="mono">{d.rules_version}</span></span>
		<span>{fmtTime(d.generated_at)}</span>
		<span style="margin-left:auto">{t('h1_snapshot')} <span class="mono">get_health_status(profile="{d.profile}")</span></span>
	</div>
{/if}

<SourcePanel />
