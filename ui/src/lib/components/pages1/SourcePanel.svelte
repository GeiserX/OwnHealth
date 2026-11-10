<script lang="ts">
	// Where a value came from: GET /api/sources/{id}, and the page image with the value's box when the
	// document service can render it. Opened by any SourceChip through openSource().
	import { getDocument, getSource, pageImageUrl } from '#lib/api';
	import type { DocumentDetail, SourceDetail } from '#lib/api/types';
	import { t } from '#lib/i18n/index.svelte';
	import { paperlessLink } from '#lib/logic/inbox';
	import { sourceLabel } from '#lib/logic/format';
	import NotReady from './NotReady.svelte';
	import { sourcePanel } from './source.svelte';
	import { Loader } from './util.svelte';

	let dialog: HTMLDialogElement | undefined = $state();
	const src = new Loader<SourceDetail>();
	let doc = $state<DocumentDetail | null>(null);
	let imgFailed = $state(false);

	$effect(() => {
		const id = sourcePanel.src;
		if (!id || !dialog) return;
		doc = null;
		imgFailed = false;
		if (!dialog.open) dialog.showModal();
		src.run(async () => {
			const s = await getSource(id);
			// The page size comes from the document; drop it if another source was opened meanwhile.
			const current = () => sourcePanel.src === id;
			if (s.document && s.location)
				getDocument(s.document.id)
					.then((d) => current() && (doc = d))
					.catch(() => current() && (doc = null));
			return s;
		}, id);
	});

	function close() {
		sourcePanel.src = null;
		dialog?.close();
	}

	const box = $derived.by(() => {
		const s = src.data;
		const bbox = s?.location?.bbox;
		const page = doc?.pages_info.find((p) => p.page_no === s?.location?.page);
		if (!bbox || !page) return null;
		const [x0, y0, x1, y1] = bbox;
		const pct = (v: number, of: number) => `${((v / of) * 100).toFixed(2)}%`;
		return { left: pct(x0, page.width_pt), top: pct(y0, page.height_pt), width: pct(x1 - x0, page.width_pt), height: pct(y1 - y0, page.height_pt) };
	});
	const day = (v: string | null | undefined) => (v ? v.slice(0, 10) : '—');
	// A code in words, with the code itself when the dict has no word for it (kept in a title).
	const word = (prefix: string, code: string | null | undefined) => {
		if (!code) return '';
		const k = prefix + code;
		return t(k) === k ? code : t(k);
	};
	// The date in a label such as 'OwnGenome · 2026-10-01', for a source with no document date.
	const labelDate = (label: string | null | undefined) => /\b(\d{4}-\d{2}-\d{2})\b/.exec(label ?? '')?.[1] ?? null;
</script>

<dialog bind:this={dialog} class="srcp" onclose={() => (sourcePanel.src = null)} aria-labelledby="srcp-h">
	<div class="card" style="padding:12px 14px">
		<h3 id="srcp-h">
			{src.data?.label ? sourceLabel(src.data.label, t('src_manual')) : t('h1_src_t')}
			<span class="r"><button class="btn sm" onclick={close}>{t('h1_close')}</button></span>
		</h3>
		<div class="mono dim" style="font-size:10.5px;margin:-6px 0 6px">{sourcePanel.src ?? ''}</div>
		{#if src.loading && !src.data}
			<div class="empty">{t('loading')}</div>
		{:else if src.error}
			<NotReady error={src.error} />
		{:else if src.data}
			{@const s = src.data}
			<table class="t" style="font-size:12px">
				<tbody>
					<tr><td class="dim">{t('h1_src_kind')}</td><td title={s.kind}>{word('src_k_', s.kind)}</td></tr>
					<tr><td class="dim">{t('h1_src_status')}</td><td title={s.status}>{word('src_st_', s.status)}</td></tr>
					{#if s.document}
						<tr>
							<td class="dim">{t('h1_src_doc')}</td>
							<td title={[s.document.doc_type, s.document.report_status].filter(Boolean).join(' · ')}>
								{[s.document.provider_name, word('i2_dt_', s.document.doc_type), word('src_rs_', s.document.report_status)].filter(Boolean).join(' · ')}
							</td>
						</tr>
					{/if}
					{#if s.document?.collected_at || labelDate(s.label)}
						<!-- 'Collected' is the day a sample was taken: only a document's collection date says that. -->
						<tr><td class="dim">{s.document?.collected_at ? t('h1_src_collected') : t('i2_date')}</td><td class="num" style="white-space:nowrap">{s.document?.collected_at ? day(s.document.collected_at) : labelDate(s.label)}</td></tr>
					{/if}
					{#if s.location}<tr><td class="dim">{t('h1_src_page')}</td><td class="num">{s.location.page}{s.document?.pages ? ' / ' + s.document.pages : ''}</td></tr>{/if}
					{#if s.import_ref}<tr><td class="dim">{t('h1_src_ref')}</td><td class="mono" style="font-size:11px;word-break:break-all">{s.import_ref}</td></tr>{/if}
					{#if paperlessLink(doc?.flags)}<tr><td class="dim">Paperless</td><td><a href={paperlessLink(doc?.flags)} target="_blank" rel="noopener noreferrer">{t('fu_open_paperless')} ↗</a></td></tr>{/if}
				</tbody>
			</table>
			{#if s.status === 'gone'}<div class="note" style="margin-top:8px">{t('h1_src_gone')}</div>{/if}
			{#if s.document && s.location}
				{#if imgFailed}
					<div class="empty">{t('h1_src_noimg')}</div>
				{:else}
					<div class="pg">
						<img src={pageImageUrl(s.document.id, s.location.page)} alt="{t('h1_src_page')} {s.location.page}" onerror={() => (imgFailed = true)} />
						{#if box}<div class="bb" style="position:absolute;left:{box.left};top:{box.top};width:{box.width};height:{box.height}"></div>{/if}
					</div>
				{/if}
			{/if}
		{/if}
	</div>
</dialog>

<style>
	.srcp {
		padding: 0;
		border: 0;
		background: transparent;
		color: var(--text);
		width: min(640px, 92vw);
		max-height: 88vh;
	}
	.srcp::backdrop {
		background: rgba(0, 0, 0, 0.55);
	}
	.pg {
		position: relative;
		margin-top: 10px;
		border-radius: 4px;
		overflow: hidden;
		background: #f4f2ee;
	}
	.pg img {
		display: block;
		width: 100%;
	}
</style>
