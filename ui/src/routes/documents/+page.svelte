<script lang="ts">
	// Documents vault: every file imported for the profile, newest first, by type, with full-text search
	// over the page text (needs read:documents_text; filenames are always searchable).
	import '#lib/components/pages2/pages2.css';
	import { page } from '$app/state';
	import { documentFileUrl, listDocuments, listJobs, retractDocument } from '#lib/api';
	import type { DocumentSummary, JobInfo } from '#lib/api/types';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import { day, plain } from '#lib/components/pages2/fmt';
	import { t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';

	const prof = $derived(currentProfile());
	const docs = new Loader<DocumentSummary[]>();
	// The newest import job per document: a file is "imported" only once its job is done.
	let jobs = $state<Record<string, JobInfo>>({});
	let q = $state('');
	let applied = $state('');
	let type = $state('');
	// ?q= from the top-bar search starts the search with that term.
	const urlQ = $derived(page.url.searchParams.get('q') ?? '');
	$effect(() => {
		q = urlQ;
		applied = urlQ;
	});
	let retracting = $state<string | null>(null);
	let reason = $state('');
	let actionError = $state<string | null>(null);

	function load() {
		const slug = prof?.slug;
		if (!slug) return;
		docs.run(async () => (await listDocuments(slug, { q: applied || undefined, type: type || undefined })).items, slug);
		listJobs()
			.then((r) => {
				const m: Record<string, JobInfo> = {};
				for (const j of r.jobs) if (j.document_id && !m[j.document_id]) m[j.document_id] = j;
				jobs = m;
			})
			.catch(() => (jobs = {}));
	}
	$effect(() => {
		void prof?.slug;
		void applied;
		void type;
		load();
	});
	// Types seen in the unfiltered list, so the chips do not vanish once one is picked.
	let seenTypes = $state<string[]>([]);
	$effect(() => {
		for (const d of docs.data ?? []) if (d.doc_type && !seenTypes.includes(d.doc_type)) seenTypes = [...seenTypes, d.doc_type];
	});
	$effect(() => {
		void prof?.slug;
		seenTypes = [];
		type = '';
	});
	const canText = $derived(prof?.scopes.includes('read:documents_text') ?? false);
	const rows = $derived(docs.data ?? []);
	const byYear = $derived.by(() => {
		const out: [string, DocumentSummary[]][] = [];
		for (const d of rows) {
			const y = d.collected_at?.slice(0, 4) ?? t('i2_undated');
			const last = out[out.length - 1];
			if (last && last[0] === y) last[1].push(d);
			else out.push([y, [d]]);
		}
		return out;
	});
	let retractBusy = $state(false);
	async function retract(id: string) {
		if (retractBusy) return;
		retractBusy = true;
		actionError = null;
		try {
			await retractDocument(id, reason.trim());
			retracting = null;
			reason = '';
			load();
		} catch (e) {
			actionError = e instanceof Error ? e.message : String(e);
		} finally {
			retractBusy = false;
		}
	}
	const flagWords = (f: Record<string, unknown>) => Object.keys(f).filter((k) => k === 'possible_injection' || k === 'hidden_text');
</script>

<div class="ph">
	<div>
		<h1>{t('i2_docs')}</h1>
		<div class="sub">{prof?.display_name ?? ''} · {t('i2_docs_sub')}</div>
	</div>
	<div class="right"><PhaseChip phase="v0.1" /><a class="btn pri" href="/import-review">{t('i2_upload')}</a></div>
</div>

<form
	class="search big"
	role="search"
	onsubmit={(e) => {
		e.preventDefault();
		applied = q.trim();
	}}
>
	<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#9aa3b2" stroke-width="2" aria-hidden="true"><circle cx="11" cy="11" r="7" /><path d="m20 20-3.5-3.5" /></svg>
	<input bind:value={q} placeholder={canText ? t('i2_docs_q') : t('i2_docs_q_name')} aria-label={t('i2_docs_q')} />
	{#if applied}<button type="button" class="btn sm" onclick={() => { q = ''; applied = ''; }}>{t('i2_clear')}</button>{/if}
</form>
<div class="filters">
	<span class="lab">{t('i2_type')}</span>
	<button class="chip" class:on={!type} aria-pressed={!type} onclick={() => (type = '')}>{t('i2_all')}</button>
	{#each seenTypes as ty (ty)}<button class="chip" class:on={type === ty} aria-pressed={type === ty} onclick={() => (type = ty)}>{t('i2_dt_' + ty)}</button>{/each}
	{#if !canText}<span class="dim" style="font-size:11.5px;margin-left:8px">{t('i2_docs_notext')}</span>{/if}
</div>

{#if actionError}<div class="p2err" role="alert" style="margin-bottom:8px">{actionError}</div>{/if}

{#if docs.error}
	<NotReady error={docs.error} onretry={load} />
{:else if docs.loading && !docs.data}
	<div class="empty">{t('loading')}</div>
{:else if !rows.length}
	<div class="card"><div class="empty">{applied ? tf('i2_docs_nohit', { q: applied }) : t('i2_docs_none')}</div></div>
{:else}
	<div class="card" style="padding:6px 6px 2px">
		<table class="t">
			<thead><tr><th>{t('i2_date')}</th><th>{t('i2_type')}</th><th>{t('i2_doc')}</th><th>{t('i2_provider')}</th><th style="text-align:right">{t('i2_pages')}</th><th>{t('i2_state')}</th><th></th></tr></thead>
			<tbody>
				{#each byYear as [year, list] (year)}
					<tr class="yr-row"><td colspan="7">{year}</td></tr>
					{#each list as d (d.id)}
						<tr class:gone={d.status === 'retracted'}>
							<td class="num" style="white-space:nowrap">{day(d.collected_at)}</td>
							<td><span class="chip">{t('i2_dt_' + (d.doc_type ?? 'other'))}</span></td>
							<td>
								<a href={documentFileUrl(d.id)} target="_blank" rel="noopener">{plain(d.filename) || d.id.slice(0, 8)}</a>
								{#if d.snippet}<div class="dim snip">{plain(d.snippet).replace(/<\/?b>/g, '')}</div>{/if}
							</td>
							<td>{d.provider_name ?? '—'}</td>
							<td class="num" style="text-align:right">{d.pages ?? '—'}</td>
							<td style="white-space:nowrap">
								{#if d.status === 'retracted'}<span class="pill p-stale"><i aria-hidden="true"></i>{t('i2_retracted')}</span>
								{:else if d.open_review}<a class="pill p-watch" href="/import-review?doc={d.id}"><i aria-hidden="true"></i>{tf('i2_to_review', { n: d.open_review })}</a>
								{:else if jobs[d.id] && jobs[d.id].state !== 'done'}
									{@const j = jobs[d.id]}
									<span class="pill {j.state === 'dead' ? 'p-out' : 'p-info'}" title={j.last_error ?? ''}><i aria-hidden="true"></i>{t('i2_job_' + j.state)}</span>
								{:else}<span class="pill p-ok"><i aria-hidden="true"></i>{t('i2_imported')}</span>{/if}
								{#each flagWords(d.flags) as fw (fw)} <span class="pill p-out" title="trust: untrusted_text"><i aria-hidden="true"></i>{t('i2_flag_' + fw)}</span>{/each}
							</td>
							<td style="text-align:right;white-space:nowrap">
								{#if d.status !== 'retracted'}<button class="btn sm no" onclick={() => { retracting = retracting === d.id ? null : d.id; reason = ''; }} aria-expanded={retracting === d.id}>{t('i2_retract')}</button>{/if}
							</td>
						</tr>
						{#if retracting === d.id}
							<tr>
								<td colspan="7">
									<form class="p2row" onsubmit={(e) => { e.preventDefault(); retract(d.id); }}>
										<span class="muted" style="font-size:12px">{t('i2_retract_d')}</span>
										<input class="p2in" style="flex:1" bind:value={reason} required placeholder={t('i2_reject_reason')} aria-label={t('i2_reject_reason')} />
										<button class="btn sm no" type="submit" disabled={retractBusy}>{t('i2_retract')}</button>
										<button class="btn sm" type="button" onclick={() => (retracting = null)}>{t('i2_cancel')}</button>
									</form>
								</td>
							</tr>
						{/if}
					{/each}
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<style>
	.search.big {
		max-width: none;
		height: 40px;
		margin-bottom: 10px;
		background: var(--raised);
	}
	.yr-row td {
		font-size: 12px;
		font-weight: 650;
		color: var(--muted);
		background: transparent !important;
		padding-top: 10px;
	}
	.gone td {
		opacity: 0.55;
	}
	.snip {
		font-size: 11.5px;
		max-width: 520px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	a.pill {
		text-decoration: none;
	}
</style>
