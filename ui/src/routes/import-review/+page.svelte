<script lang="ts">
	// Mockup screen #import-review: inbox tiles, upload, and the split view (document page with the
	// extracted rows boxed on the left, the rows waiting for a person on the right).
	import '#lib/components/pages2/pages2.css';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import {
		acceptReview,
		getMe,
		addAlias,
		bulkAccept,
		dismissReview,
		documentFileUrl,
		getDocument,
		listDocuments,
		listInboxes,
		listJobs,
		listReview,
		rejectReview,
		retractDocument
	} from '#lib/api';
	import type { DocumentDetail, DocumentSummary, JobInfo, ReviewItem } from '#lib/api/types';
	import type { InboxSource } from '#lib/api/inbox';
	import AgentContext from '#lib/components/inbox/AgentContext.svelte';
	import ProposalRows from '#lib/components/inbox/ProposalRows.svelte';
	import { inboxSummary, nearDuplicateOf, othersGrouped, paperlessLink, proposalRows, rowGrounding } from '#lib/logic/inbox';
	import { ApiError } from '#lib/api/core';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import PageView, { type Box } from '#lib/components/pages2/PageView.svelte';
	import ReviewRow from '#lib/components/pages2/ReviewRow.svelte';
	import UploadPanel from '#lib/components/pages2/UploadPanel.svelte';
	import ManualEntry from '#lib/components/ManualEntry.svelte';
	import { day, plain } from '#lib/components/pages2/fmt';
	import { fmtNum, i18n, t } from '#lib/i18n/index.svelte';
	import { app, currentProfile } from '#lib/state/app.svelte';

	const THRESHOLD = 0.95;
	const prof = $derived(currentProfile());
	// sources is null when the inbox list failed for another reason than "not yours to manage".
	const queue = new Loader<{ items: ReviewItem[]; docs: DocumentSummary[]; jobs: JobInfo[]; sources: InboxSource[] | null }>();
	// "Not a duplicate? reject" waits for a second click: it retracts this document.
	let confirmDup = $state(false);
	const doc = new Loader<DocumentDetail>();
	let showUpload = $state(false);
	let busy = $state(false);
	let actionError = $state<{ text: string; raw: string } | null>(null);
	// "Document withdrawn: …" after a person rejects a whole document.
	let notice = $state<string | null>(null);
	let selected = $state<string | null>(null);
	let pageNo = $state(1);
	let zoom = $state(1);
	let checked = $state<Record<string, boolean>>({});
	let rejectingDoc = $state(false);
	let docReason = $state('');
	// A photo or scanned PDF (pending_document): its values are typed in by hand, then the item closes.
	let entering = $state<string | null>(null);
	// The observations saved for the scan being entered; "Done" needs at least one.
	let enteredIds = $state<string[]>([]);
	let confirmDismiss = $state<string | null>(null);
	function enter(id: string | null) {
		entering = id;
		enteredIds = [];
		confirmDismiss = null;
	}
	const docName = (id: string | null) => {
		const d = (queue.data?.docs ?? []).find((x) => x.id === id);
		return d ? docLabel(d) : '';
	};

	function load() {
		const slug = prof?.slug;
		if (!slug) return;
		queue.run(async () => {
			const [r, d, j, s] = await Promise.all([
				listReview({ status: 'open', profile: slug }),
				listDocuments(slug).catch(() => ({ items: [] as DocumentSummary[] })),
				listJobs().catch(() => ({ jobs: [] as JobInfo[] })),
				// The inbox tile; a viewer who cannot manage sources (403) just sees the setup tile. Any other
				// failure leaves the tile at '—' rather than claiming there is no inbox.
				listInboxes().catch((e) => (e instanceof ApiError && e.status === 403 ? { items: [] as InboxSource[] } : { items: null }))
			]);
			return { items: r.items, docs: d.items, jobs: j.jobs, sources: s.items };
		}, slug);
	}
	$effect(() => {
		void prof?.slug;
		load();
	});

	const items = $derived(queue.data?.items ?? []);
	// An agent's observations proposal is shown row by row next to its document, like an import.
	const isObsProposal = (i: ReviewItem) => i.kind === 'agent_proposal' && !!i.document_id && proposalRows(i) != null;
	const docsWithRows = $derived.by(() => {
		const ids = new Set(items.filter((i) => (i.kind === 'candidate_row' || isObsProposal(i)) && i.document_id).map((i) => i.document_id as string));
		return (queue.data?.docs ?? []).filter((d) => ids.has(d.id));
	});
	// Links from MCP: get_source.crop_url (?doc=&page=&src=) and propose.review_url (?item=).
	const itemParam = $derived(page.url.searchParams.get('item'));
	// An item link opens its document when the item is one of its rows; any other item (a scan waiting
	// for values, a proposal with no document) opens in 'Other items' and the first document with rows shows.
	const itemDoc = $derived.by(() => {
		const it = items.find((i) => i.id === itemParam);
		return it && (it.kind === 'candidate_row' || isObsProposal(it)) ? it.document_id : null;
	});
	const docId = $derived(page.url.searchParams.get('doc') ?? itemDoc ?? docsWithRows[0]?.id ?? null);
	// The select always holds the open document, also one with no rows left (a scan, a link from MCP).
	const docOptions = $derived.by(() => {
		if (!docId || docsWithRows.some((d) => d.id === docId)) return docsWithRows;
		const d = (queue.data?.docs ?? []).find((x) => x.id === docId);
		return d ? [...docsWithRows, d] : docsWithRows;
	});
	// A link to a scan waiting for values opens its entry form once.
	let autoOpened: string | null = null;
	$effect(() => {
		const it = items.find((i) => i.id === itemParam);
		if (it?.kind === 'pending_document' && autoOpened !== it.id) {
			autoOpened = it.id;
			enter(it.id);
		}
	});
	// The two-click "reject as duplicate" belongs to one document: another document starts unarmed.
	$effect(() => {
		void docId;
		confirmDup = false;
	});
	$effect(() => {
		const id = docId;
		if (!id) return;
		selected = itemParam;
		checked = {};
		pageNo = Number(page.url.searchParams.get('page')) || 1;
		doc.run(() => getDocument(id), id);
	});
	const rows = $derived(items.filter((i) => i.kind === 'candidate_row' && i.document_id === docId));
	const docProposals = $derived(items.filter((i) => isObsProposal(i) && i.document_id === docId).map((i) => ({ item: i, rows: proposalRows(i) ?? [] })));
	// v0.2: "probably the same as document X" (dedup level 2) is a banner over that document's rows.
	const near = $derived(nearDuplicateOf(items, docId));
	const others = $derived(
		othersGrouped(items.filter((i) => !((i.kind === 'candidate_row' || isObsProposal(i)) && i.document_id === docId) && i.id !== near?.id))
	);
	const nearOf = $derived.by(() => {
		const id = (near?.payload as { same_as?: string } | undefined)?.same_as;
		return id ? { id, doc: (queue.data?.docs ?? []).find((x) => x.id === id) ?? null } : null;
	});
	const docNameOf = (id: string) => {
		const d = (queue.data?.docs ?? []).find((x) => x.id === id);
		return d ? docLabel(d) : '';
	};
	const pg = (i: ReviewItem) => Number((i.payload as { page_no?: number }).page_no ?? 1);
	const pinfo = $derived(doc.data?.pages_info.find((p) => p.page_no === pageNo) ?? null);
	const boxes = $derived<Box[]>([
		...rows
			.filter((i) => pg(i) === pageNo && Array.isArray((i.payload as { bbox?: number[] }).bbox))
			.map((i) => ({
				id: i.id,
				bbox: (i.payload as { bbox: number[] }).bbox,
				tone: (i.reasons.includes('possible_injection') || rowGrounding(i) === 'ungrounded' ? 'bad' : i.reasons.length ? 'low' : '') as Box['tone'],
				label: String(((i.payload as { observation?: { printed_name?: string } }).observation ?? {}).printed_name ?? '')
			})),
		// An agent's rows the server found on this page; a row not found has no box to draw.
		...docProposals.flatMap((p) =>
			p.rows
				.filter((r) => r.bbox && r.page === pageNo)
				.map((r) => ({ id: `${p.item.id}:${r.index}`, bbox: r.bbox as number[], tone: (r.problems.length ? 'low' : '') as Box['tone'], label: r.name }))
		)
	]);
	// Pages carrying text aimed at an AI or hidden text (lane B's flags on the page and on the document).
	const flagged = $derived.by(() => {
		const d = doc.data;
		if (!d) return [] as number[];
		const pages = new Set(d.pages_info.filter((p) => p.flags && ('possible_injection' in p.flags || 'hidden_text' in p.flags)).map((p) => p.page_no));
		for (const n of (d.flags?.hidden_text as number[] | undefined) ?? []) pages.add(n);
		return [...pages].sort((a, b) => a - b);
	});
	const paperlessUrl = $derived(paperlessLink(doc.data?.flags));
	const untrusted = $derived(!!doc.data?.flags?.possible_injection || flagged.length > 0 || rows.some((r) => r.reasons.includes('possible_injection')));
	const duplicate = $derived(rows.flatMap((i) => ((i.payload as { checks?: { reason: string | null; detail: string | null }[] }).checks ?? []).filter((c) => c.reason === 'duplicate'))[0] ?? null);
	const bulkable = $derived(rows.filter((i) => !i.reasons.length && (i.row_conf ?? 0) >= THRESHOLD).length);
	const picked = $derived(rows.filter((i) => checked[i.id]).map((i) => i.id));
	// Only this document's rows: the footer must not claim an agent proposed rows a PDF import produced.
	const agentCount = $derived(
		rows.filter((i) => i.proposed_by_token != null).length + items.filter((i) => i.kind === 'agent_proposal' && i.document_id === docId).length
	);

	function pick(id: string) {
		selected = id;
		const it = rows.find((i) => i.id === id);
		if (it) pageNo = pg(it);
		requestAnimationFrame(() => document.querySelector('.box.on')?.scrollIntoView({ block: 'nearest', inline: 'nearest' }));
		document.getElementById(`rr-${id.slice(0, 8)}-n`)?.scrollIntoView({ block: 'nearest' });
	}

	// `inRow`: the caller shows the error next to its own control, so the failure is thrown back to it.
	async function act(fn: () => Promise<unknown>, inRow = false) {
		busy = true;
		actionError = null;
		notice = null;
		try {
			await fn();
		} catch (e) {
			if (inRow) throw e;
			actionError = { text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
			// Also after a failure: a batch that failed at row N already accepted rows 1 to N-1.
			load();
			refreshReviewTotal();
		}
	}
	// The top bar and the sidebar show the household total; refresh it from /me after every decision.
	// If that call fails the old number stays, and the queue's own load shows the error.
	function refreshReviewTotal() {
		getMe()
			.then((m) => {
				if (app.me) app.me.review_open = m.review_open;
			})
			.catch(() => undefined);
	}
	const acceptPicked = () => act(async () => { for (const id of picked) await acceptReview(id); });
	const rowAct = (fn: () => Promise<unknown>) => act(fn, true);
	async function rejectDoc() {
		const id = docId;
		if (!id) return;
		const name = docName(id) || (doc.data ? plain(doc.data.filename) : '');
		await act(async () => {
			await retractDocument(id, docReason.trim());
			rejectingDoc = false;
			docReason = '';
			await goto('?');
			notice = tf('i2_doc_retracted', { name });
		});
	}
	// Close the scan's item with the values typed from it, so the audit says which ones.
	const doneEntering = (it: ReviewItem) =>
		act(async () => {
			await dismissReview(it.id, `entered by hand: ${enteredIds.join(', ')}`);
			enter(null);
		});

	// Inbox tiles: what each channel left in the queue, and its newest import.
	const latest = (kind: string) => (queue.data?.jobs ?? []).find((j) => j.kind === kind) ?? null;
	const latestDoc = (type: string) => (queue.data?.docs ?? []).find((d) => d.doc_type === type) ?? null;
	const count = (...kinds: string[]) => items.filter((i) => kinds.includes(i.kind)).length;
	const tiles = $derived([
		{ k: 'i2_in_lab', d: latest('import.pdf'), doc: latestDoc('lab_report'), n: count('candidate_row', 'document_identity', 'amended_report', 'pending_document') },
		// An Apple Health export goes straight in; nothing from it waits for review.
		{ k: 'i2_in_apple', d: latest('import.apple_health'), doc: (queue.data?.docs ?? []).find((x) => x.flags?.apple_health) ?? null, n: 0 },
		{ k: 'i2_in_fhir', d: latest('import.fhir_bundle'), n: count('fhir_unmapped') },
		{ k: 'i2_in_genome', d: latest('import.genome_facts'), doc: latestDoc('genomics'), n: count('genome_reinterpretation') },
		{ k: 'i2_in_agent', d: null, n: count('agent_proposal'), sub: 'write:propose' },
	]);
	// v0.2: one Inbox tile, as the mockup draws it: how many sources, how many failing, and the held mail
	// (the server's count). The per-source detail lives on the Inbox screen. No inbox yet: a setup tile.
	const ready = $derived(!!queue.data);
	const inbox = $derived(queue.data ? inboxSummary(queue.data.sources) : null);
	function jobLine(j: JobInfo | null, d?: DocumentSummary | null): string {
		const doc = j?.document_id ? (queue.data?.docs ?? []).find((x) => x.id === j.document_id) : d;
		const name = j?.filename ?? (doc ? plain(doc.filename) : null);
		const when = j ? day(j.finished_at ?? j.created_at) : doc?.collected_at ? day(doc.collected_at) : null;
		return name || when ? [name, when].filter(Boolean).join(' · ') : t('i2_in_none');
	}

	const docOf = (id: string | null) => (queue.data?.docs ?? []).find((x) => x.id === id) ?? null;
	const docLabel = (d: DocumentSummary) => `${plain(d.filename) || d.provider_name || d.id.slice(0, 8)}`;
	const kindWord = (prefix: string, v: string) => (t(prefix + v) !== prefix + v ? t(prefix + v) : v);
	function otherTitle(i: ReviewItem): string {
		const p = i.payload as Record<string, unknown>;
		if (i.kind === 'agent_proposal') return tf('i2_ag_t', { kind: kindWord('i2_pk_', String(p.kind ?? '')), token: String(p.token_label ?? '—') });
		if (i.kind === 'job_failed') return tf('i2_jf_t', { file: String(p.filename ?? p.kind ?? '') });
		if (i.kind === 'candidate_row') {
			const d = (queue.data?.docs ?? []).find((x) => x.id === i.document_id);
			return tf('i2_cr_t', { name: String((p.observation as { printed_name?: string })?.printed_name ?? ''), doc: d ? docLabel(d) : '—' });
		}
		// A scan's chip already says what it is; the line names the file.
		if (i.kind === 'pending_document' && i.document_id && docName(i.document_id)) return docName(i.document_id);
		return t('i2_kind_' + i.kind);
	}
	function otherText(i: ReviewItem): string {
		const p = i.payload as Record<string, unknown>;
		const inner = p.payload as Record<string, unknown> | undefined;
		if (i.kind === 'agent_proposal') return String(inner?.text ?? JSON.stringify(inner ?? {}));
		if (i.kind === 'job_failed') {
			const err = String(p.error ?? p.last_error ?? '');
			// the server's code for a locked PDF: say what to do in the reader's language
			return err.includes('pdf_password:') ? t('ir_pdf_password') : errText(new Error(err));
		}
		return '';
	}
</script>

<PageHeader title={t('nav_imp')} sub={t('i2_imp_sub')}>
	{#snippet right()}
		<PhaseChip phase={t('i2_imp_ph1')} /><PhaseChip phase={t('i2_imp_ph2')} />
		<button class="btn pri" onclick={() => (showUpload = !showUpload)} aria-expanded={showUpload}>{t('i2_upload')}</button>
	{/snippet}
</PageHeader>

{#if showUpload && prof}
	<UploadPanel
		profile={prof.slug}
		profileName={prof.display_name}
		rowsOf={(id) => items.filter((i) => i.kind === 'candidate_row' && i.document_id === id).length}
		ondone={(ids) => {
			load();
			refreshReviewTotal();
			if (ids[0]) goto(`?doc=${ids[0]}`);
		}}
	/>
{/if}

<div class="inbox">
	{#each tiles as b (b.k)}
		<div class="ib">
			<div style="min-width:0">
				<div style="font-weight:600;font-size:12.5px">{t(b.k)}</div>
				<div class="dim" style="font-size:11px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">{b.sub ?? (ready ? jobLine(b.d, b.doc) : '')}&nbsp;</div>
			</div>
			{#if !ready}<span class="c dim" aria-label={t('loading')}>—</span>{:else if b.n != null}<span class="c" style="color:{b.n ? 'var(--watch)' : 'var(--dim)'}" aria-label="{b.n} {t('i2_open_items')}">{b.n}</span>{/if}
		</div>
	{/each}
	{#if ready && inbox && !inbox.sources}
		<a class="ib lnkt" href="/inbox">
			<div style="min-width:0">
				<div style="font-weight:600;font-size:12.5px">{t('i2_inbox_tile')} <PhaseChip phase="v0.2" /></div>
				<div class="dim" style="font-size:11px">{t('fu_tile_setup')}</div>
			</div>
		</a>
	{:else}
		<a class="ib lnkt" href="/inbox">
			<div style="min-width:0">
				<div style="font-weight:600;font-size:12.5px">{t('i2_inbox_tile')}</div>
				<div class="dim" style="font-size:11px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">
					{#if inbox}{tp('fu_tile_sources', inbox.sources)}{#if inbox.failing}{' · '}<span style="color:var(--out)">{tf('fu_tile_failing', { n: inbox.failing })}</span>{/if}{/if}&nbsp;
				</div>
			</div>
			{#if inbox}<span class="c" style="color:{inbox.held ? 'var(--watch)' : 'var(--dim)'}" title={t('fu_quarantine')} aria-label="{inbox.held} {t('fu_quarantine')}">{inbox.held}</span>{:else}<span class="c dim" aria-label={t('loading')}>—</span>{/if}
		</a>
	{/if}
</div>

{#if actionError}<div class="p2err" role="alert" style="margin-bottom:8px" title={actionError.raw}>{actionError.text}</div>{/if}
{#if notice}<div class="p2ok" role="status" style="margin-bottom:8px;font-size:12px">{notice}</div>{/if}

{#if queue.error}
	<NotReady error={queue.error} onretry={load} />
{:else if queue.loading && !queue.data}
	<div class="empty">{t('loading')}</div>
{:else if !docId}
	<div class="card"><div class="empty">{items.length ? t('i2_no_doc_rows') : t('i2_queue_empty')} <a href="/documents">{t('i2_docs_link')}</a></div></div>
{:else}
	<div class="imp">
		<div>
			<div class="dochd">
				{#if docOptions.length > 1}
					<select class="p2in" style="max-width:220px;font-weight:600" aria-label={t('i2_doc')} value={docId} onchange={(e) => goto(`?doc=${(e.currentTarget as HTMLSelectElement).value}`)}>
						{#each docOptions as d (d.id)}<option value={d.id}>{docLabel(d)}</option>{/each}
					</select>
				{:else if doc.data}
					<b style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">{plain(doc.data.filename)}</b>
				{/if}
				{#if doc.data}
					<span class="dim num" style="font-size:12px;white-space:nowrap;flex:1 1 0;min-width:0;overflow:hidden;text-overflow:ellipsis">{[doc.data.provider_name, day(doc.data.collected_at), pinfo ? (pinfo.text_layer_ok ? t('i2_textlayer') : t('i2_ocr')) : null].filter(Boolean).join(' · ')}</span>
					{#if doc.data.pages_info.length}<button class="btn sm" style="margin-left:auto" onclick={() => (zoom = zoom >= 2 ? 1 : zoom + 0.5)} title={t('i2_zoom')} aria-label="{t('i2_zoom')} {zoom}×">{zoom}×</button>
					<span class="seg" role="group" aria-label={t('i2_page')}>
						{#each doc.data.pages_info as p (p.page_no)}
							<button class:on={p.page_no === pageNo} aria-pressed={p.page_no === pageNo} onclick={() => (pageNo = p.page_no)}>p{p.page_no}{#if flagged.includes(p.page_no)} <span aria-label={t('i2_flag_possible_injection')}>⚠</span>{/if}</button>
						{/each}
					</span>{/if}
				{/if}
			</div>
			{#if doc.error}
				<NotReady error={doc.error} />
			{:else if doc.data && pinfo && docId}
				<PageView documentId={docId} page={pageNo} width={pinfo.width_pt} height={pinfo.height_pt} {boxes} {selected} {zoom} onpick={pick} />
			{:else if doc.data && docId && doc.data.mime.startsWith('image/')}
				<img class="scan" src={documentFileUrl(docId)} alt={tf('i2_scan_alt', { doc: plain(doc.data.filename) })} />
				<div class="note" style="margin-top:6px">{t('i2_no_pages_image')}</div>
			{:else if doc.data && !doc.data.pages_info.length}
				<div class="empty">{t('i2_no_pages')}</div>
			{:else}
				<div class="empty">{t('loading')}</div>
			{/if}
		</div>

		<div class="card" style="padding:0;overflow:hidden;align-self:start">
			<div class="rowshd">
				<b style="white-space:nowrap">{t('i2_rows')}</b>
				<span class="dim" style="font-size:12px;white-space:nowrap">{tf('i2_rows_need', { n: rows.length })}</span>
				<span class="rowacts">
					{#if paperlessUrl}<a class="btn sm" href={paperlessUrl} target="_blank" rel="noopener noreferrer">{t('fu_open_paperless')} ↗</a>{/if}
					{#if picked.length}<button class="btn sm ok" onclick={acceptPicked} disabled={busy}>{tf('i2_accept_sel', { n: picked.length })}</button>{/if}
					<!-- A document with nothing left to decide offers neither: there is nothing to accept or withdraw here. -->
					{#if rows.length}
						<button class="btn sm" onclick={() => docId && act(() => bulkAccept(docId, THRESHOLD))} disabled={busy || !bulkable} title={t('i2_bulk_d')}>{t('i2_bulk')} ≥ {fmtNum(THRESHOLD, 2)}</button>
						<button class="btn sm no" onclick={() => (rejectingDoc = !rejectingDoc)} aria-expanded={rejectingDoc}>{t('i2_reject_doc')}</button>
					{/if}
				</span>
			</div>
			{#if rejectingDoc && rows.length}
				<div class="rejdoc">
					<input class="p2in" bind:value={docReason} required placeholder={t('i2_reject_reason')} aria-label={t('i2_reject_reason')} />
					<ConfirmRow
						text={tf('i2_reject_doc_confirm', { name: docName(docId) || '—' })}
						confirmLabel={t('i2_reject_doc')}
						busy={busy || !docReason.trim()}
						onconfirm={rejectDoc}
						oncancel={() => (rejectingDoc = false)}
					/>
				</div>
			{/if}
			{#if near && docId}
				<div class="banner dupb" role="note" style="border-color:var(--line2);background:var(--raised)">
					<span class="muted" style="font-size:12px">{t('i2_dup')} {nearOf?.doc ? tf('fu_dup_doc', { doc: docLabel(nearOf.doc), date: day(nearOf.doc.collected_at) }) : t('fu_dup_unknown')} {tf('fu_dup_pages', { n: Number((near.payload as { pages?: number }).pages ?? 0) })}</span>
					<span class="p2row" style="margin-left:auto;gap:6px">
						{#if nearOf}<a class="btn sm" href="?doc={nearOf.id}">{t('fu_dup_compare')}</a>{/if}
						<button class="btn sm" disabled={busy} onclick={() => near && act(() => dismissReview(near.id, 'kept both'))}>{t('fu_dup_keep')}</button>
						{#if confirmDup}
							<button class="btn sm no" disabled={busy} onclick={() => act(async () => { confirmDup = false; await retractDocument(docId, `duplicate of ${nearOf?.id ?? 'an earlier document'}`); if (near) await dismissReview(near.id, 'duplicate retracted'); })}>{t('fu_dup_reject_yes')}</button>
							<button class="btn sm" onclick={() => (confirmDup = false)}>{t('i2_cancel')}</button>
						{:else}
							<button class="btn sm no" disabled={busy} onclick={() => (confirmDup = true)}>{t('fu_dup_reject')}</button>
						{/if}
					</span>
				</div>
				{#if confirmDup}<div class="note" style="margin:0 12px 8px">{t('fu_dup_reject_d')}</div>{/if}
			{/if}
			{#if duplicate}
				<div class="banner" style="margin:10px 12px;padding:7px 10px;border-color:var(--line2);background:var(--raised)">
					<span class="muted" style="font-size:12px">{t('i2_dup')} {duplicate.detail ?? ''}</span>
				</div>
			{/if}
			<div class="xr h" aria-hidden="true"><span></span><span>{t('i2_h_name')}</span><span>{t('i2_h_loinc')}</span><span>{t('i2_h_value')}</span><span>{t('i2_h_conf')}</span><span style="text-align:right">{t('i2_h_act')}</span></div>
			{#each rows as it (it.id)}
				<ReviewRow
					item={it}
					threshold={THRESHOLD}
					selected={selected === it.id}
					checked={!!checked[it.id]}
					oncheck={(on) => (checked[it.id] = on)}
					{busy}
					docName={docNameOf}
					onselect={() => pick(it.id)}
					onaccept={(edits) => rowAct(() => acceptReview(it.id, edits))}
					onreject={(reason) => rowAct(() => rejectReview(it.id, reason))}
					ondiscard={() => rowAct(() => dismissReview(it.id, 'untrusted text'))}
					onalias={(b) => rowAct(async () => prof && addAlias({ profile: prof.slug, alias: String((it.payload as { observation?: { printed_name?: string } }).observation?.printed_name ?? ''), alias_group: b.alias_group, loinc: b.loinc, lang: i18n.lang }))}
				/>
			{:else}
				{#if !docProposals.length}<div class="empty">{t('i2_doc_done')}</div>{/if}
			{/each}
			{#each docProposals as p (p.item.id)}
				<div class="prop">
					<div style="font-size:12px"><span class="chip" style="color:var(--accent);border-color:rgba(110,168,254,.5)">{t('i2_agent_chip')}</span> ◆ {otherTitle(p.item)}</div>
					<ProposalRows rows={p.rows} {busy} onaccept={(edits) => act(() => acceptReview(p.item.id, edits))} onreject={() => act(() => rejectReview(p.item.id, 'rejected in review'))} />
					<AgentContext item={p.item} docName={docNameOf} />
				</div>
			{/each}
			<div style="padding:10px 12px;display:flex;flex-direction:column;gap:6px">
				{#if agentCount}
					<div class="muted" style="font-size:11.5px"><span class="chip" style="color:var(--accent);border-color:rgba(110,168,254,.5)">{t('i2_agent_chip')}</span> {t('i2_agent_note')}</div>
				{/if}
				{#if untrusted}
					<div class="muted" style="font-size:11.5px" role="note">
						<span class="chip" style="color:var(--out);border-color:rgba(239,107,115,.5)" title={t('fu_untrusted_title')}>{t('fu_untrusted_chip')}</span>
						{flagged.length ? tf('i2_untrusted_note', { pages: flagged.join(', ') }) : t('i2_untrusted_doc')} <span class="mono">trust: untrusted_text</span>{t('i2_untrusted_note2')}
					</div>
				{/if}
			</div>
		</div>
	</div>
{/if}

{#if others.length}
	<div class="card" style="margin-top:12px;padding:8px 6px 2px">
		<h3 style="padding:0 8px">{t('i2_others')}<span class="r">{tf('i2_others_d', { n: others.reduce((n, l) => n + (l.rows?.length ?? 1), 0) })}</span></h3>
		<div class="tscroll">
		<table class="t stack others">
			<tbody>
				{#each others as line (line.item.id)}
					{@const it = line.item}
					{@const prows = proposalRows(it)}
					<tr class:agent={it.kind === 'agent_proposal'} id="oi-{it.id.slice(0, 8)}">
						<td class="kindcol" style="width:1%"><span class="chip" title={it.kind} style={it.kind === 'agent_proposal' ? 'color:var(--accent);border-color:rgba(110,168,254,.5)' : ''}>{t('i2_kind_' + it.kind)}</span></td>
						<td class="mtxt">
							{#if line.rows}
								<div>{tp('i2_doc_rows', line.rows.length, { doc: docNameOf(line.document_id) || '—' })}</div>
								<div class="dim" style="font-size:11.5px">{line.rows.map((r) => String((r.payload as { observation?: { printed_name?: string } }).observation?.printed_name ?? '')).filter(Boolean).join(' · ')}</div>
							{:else}
								<div>{it.kind === 'agent_proposal' ? '◆ ' : ''}{otherTitle(it)}</div>
								{#if prows}
									<ProposalRows rows={prows} {busy} documentId={it.document_id} onaccept={(edits) => act(() => acceptReview(it.id, edits))} onreject={() => act(() => rejectReview(it.id, 'rejected in review'))} />
								{:else if otherText(it)}<pre class="p2untrusted" style="margin-top:4px">{otherText(it)}</pre>{/if}
								{#if it.kind === 'agent_proposal'}<AgentContext item={it} docName={docNameOf} />{:else if it.context_src_ids?.length}<div class="dim mono" style="font-size:10.5px">{t('i2_based_on')} {it.context_src_ids.join(', ')}</div>{/if}
							{/if}
						</td>
						<td class="dim"><span class="num" style="white-space:nowrap">{day(it.created_at)}</span></td>
						<td class="acts-cell" style="white-space:nowrap;text-align:right">
							{#if line.rows}
								<a class="btn sm" href="?doc={line.document_id}">{t('i2_open_doc')}</a>
							{:else if it.kind === 'pending_document' && prof}
								{#if it.document_id}<a class="btn sm" href={documentFileUrl(it.document_id)} target="_blank" rel="noopener">{t('i2_open_doc')}</a>{/if}
								{#if entering === it.id}
									<button class="btn sm pri" onclick={() => doneEntering(it)} disabled={busy || !enteredIds.length}>{t('me_done')}</button>
									<button class="btn sm" onclick={() => enter(null)} aria-expanded="true">{t('h1_close')}</button>
								{:else}
									<button class="btn sm pri" onclick={() => enter(it.id)} aria-expanded="false">{t('me_enter')}</button>
								{/if}
								<button class="btn sm no" onclick={() => (confirmDismiss = it.id)} disabled={busy}>{t('i2_dismiss')}</button>
							{:else if (it.kind === 'agent_proposal' && !prows) || it.kind === 'fhir_unmapped'}
								<button class="btn sm ok" onclick={() => act(() => acceptReview(it.id))} disabled={busy}>{t('i2_accept')}</button>
								<button class="btn sm no" onclick={() => act(() => rejectReview(it.id, 'rejected in review'))} disabled={busy}>{t('i2_reject')}</button>
							{:else}
								<button class="btn sm" onclick={() => act(() => dismissReview(it.id))} disabled={busy}>{t('i2_dismiss')}</button>
							{/if}
						</td>
					</tr>
					{#if confirmDismiss === it.id}
						<tr class="sub"><td colspan="4">
							<ConfirmRow text={t('i2_scan_dismiss_d')} confirmLabel={t('i2_dismiss')} {busy} onconfirm={() => act(async () => { await dismissReview(it.id); enter(null); })} oncancel={() => (confirmDismiss = null)} />
						</td></tr>
					{/if}
					{#if entering === it.id && prof}
						<tr class="sub"><td colspan="4">
							<div class="entry">
								{#if it.document_id}<img class="scan" src={documentFileUrl(it.document_id)} alt={tf('i2_scan_alt', { doc: docName(it.document_id) || '—' })} />{/if}
								<div style="min-width:0">
									<div class="note" style="margin-bottom:6px">{t('i2_scan_d')}{#if enteredIds.length} <b class="p2ok">{tp('i2_scan_saved', enteredIds.length)}</b>{/if}</div>
									<ManualEntry
										profile={prof.slug}
										kinds={['observation', 'medication', 'immunization']}
										note={tf('me_from_doc', { doc: docName(it.document_id) || '—' })}
										date={(it.payload as { collected_at?: string | null }).collected_at?.slice(0, 10) ?? docOf(it.document_id)?.collected_at?.slice(0, 10) ?? ''}
										onsaved={(_k, id) => {
											if (id) enteredIds = [...enteredIds, id];
										}}
										oncancel={() => enter(null)}
									/>
								</div>
							</div>
						</td></tr>
					{/if}
				{/each}
			</tbody>
		</table>
		</div>
	</div>
{/if}

<style>
	.seg button {
		white-space: nowrap;
	}
	tr.agent td {
		background: rgba(110, 168, 254, 0.05);
	}
	.imp {
		grid-template-columns: 430px minmax(0, 1fr);
		align-items: start;
	}
	.dochd {
		display: flex;
		gap: 8px;
		align-items: center;
		margin-bottom: 8px;
		flex-wrap: wrap;
	}
	.rowshd {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 10px 12px;
		border-bottom: 1px solid var(--line);
		flex-wrap: wrap;
	}
	.rowacts {
		margin-left: auto;
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
		justify-content: flex-end;
	}
	.rejdoc {
		display: grid;
		gap: 6px;
		padding: 8px 12px;
		border-bottom: 1px solid var(--line);
	}
	.scan {
		display: block;
		max-width: 100%;
		max-height: 70vh;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: #fff;
	}
	.entry {
		display: grid;
		grid-template-columns: minmax(0, 280px) minmax(0, 1fr);
		gap: 12px;
		align-items: start;
	}
	.entry .scan {
		max-height: 420px;
	}
	tr.sub td {
		background: var(--raised);
	}
	.dupb {
		margin: 10px 12px;
		padding: 7px 10px;
		flex-wrap: wrap;
	}
	/* The tiles wrap onto more rows instead of running off the page; at 1440 the six still fit one row. */
	.inbox {
		flex-wrap: wrap;
	}
	.inbox .ib {
		flex: 1 1 170px;
		min-width: 0;
	}
	@media (max-width: 1100px) {
		.imp {
			grid-template-columns: minmax(0, 1fr);
		}
		/* Tablet: the text gets the width; the kind chip goes (the title line says it) and the buttons wrap. */
		.others td.kindcol {
			display: none;
		}
		.others td.acts-cell {
			white-space: normal !important;
			width: 30%;
		}
	}
	@media (max-width: 760px) {
		.entry {
			grid-template-columns: minmax(0, 1fr);
		}
		/* The raw kind chip column says nothing a person needs on a phone: the title line says it. */
		.others td.kindcol {
			display: none !important;
		}
		.others td.mtxt {
			display: block;
			width: 100%;
			margin: 0;
			color: var(--text);
			font-size: 12.5px;
			font-weight: 560;
		}
		/* A stacked row is one block: its buttons take the full width (right-aligned), and the agent
		   tint covers the whole row, not each stacked cell. */
		.others td.acts-cell {
			width: 100%;
		}
		tr.agent {
			background: rgba(110, 168, 254, 0.05);
		}
		tr.agent td {
			background: none;
		}
	}
	a.lnkt {
		color: inherit;
	}
	.prop {
		padding: 8px 12px;
		border-top: 1px solid var(--line);
		background: rgba(110, 168, 254, 0.05);
	}
	a.lnkt:hover {
		border-color: var(--line2);
	}
</style>
