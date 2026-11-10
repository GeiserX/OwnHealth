<script lang="ts">
	// One extracted row waiting in the review queue: the mockup's `.xr` grid, with accept, edit,
	// reject (with a reason), "add alias" for unknown names, and quarantine for untrusted text.
	import type { Check, ReviewItem } from '#lib/api/types';
	import { fmtNum, i18n, t } from '#lib/i18n/index.svelte';
	import { errRaw, errText, tf } from '#lib/components/pages1/util.svelte';
	import AgentContext from '#lib/components/inbox/AgentContext.svelte';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { repeatsLabel, reviewChecks, rowGrounding } from '#lib/logic/inbox';
	import { decimalText, unitLabel } from '#lib/logic/format';
	import { dict } from '#lib/i18n/dict';
	import { day } from './fmt';

	type Obs = Record<string, string | number | null | undefined>;
	let {
		item,
		selected = false,
		checked = false,
		oncheck,
		busy = false,
		threshold = 0.95,
		onselect,
		onaccept,
		onreject,
		ondiscard,
		onalias,
		docName = () => ''
	}: {
		item: ReviewItem;
		selected?: boolean;
		checked?: boolean;
		oncheck: (on: boolean) => void;
		busy?: boolean;
		threshold?: number;
		onselect: () => void;
		// Each returns its request: the row keeps its form open and shows the error here when it fails.
		onaccept: (edits?: Record<string, unknown>) => Promise<unknown>;
		onreject: (reason: string) => Promise<unknown>;
		ondiscard: () => Promise<unknown>;
		onalias: (body: { alias_group: string; loinc: string | null }) => Promise<unknown>;
		docName?: (id: string) => string;
	} = $props();

	const pl = $derived(item.payload as { observation?: Obs; checks?: Check[]; cells?: Record<string, { text?: string }> });
	const obs = $derived(pl.observation ?? {});
	// The grounding reasons have their own line below; keep them out of the generic check list.
	const GROUNDING_REASONS = ['not_grounded', 'ungrounded_no_word_layer'];
	// One line per reason: the server can file the same reason twice (a conflict from two places).
	const failed = $derived(reviewChecks(pl.checks ?? [], GROUNDING_REASONS));
	const quarantined = $derived(item.reasons.includes('possible_injection'));
	const unknownName = $derived(item.reasons.some((r) => r === 'loinc_unknown' || r === 'unmapped'));
	// A value not found on the page has confidence 0, whatever the extractor scored: the bar says so too.
	const conf = $derived(rowGrounding(item) === 'ungrounded' ? 0 : (item.row_conf ?? 0));
	const lowConf = $derived(item.row_conf != null && item.row_conf < threshold);
	// v0.2: a value not found in the page's word layer is red (confidence 0); a page with no word layer
	// (a scan while OCR is off) is amber, and the person compares each digit with the page image.
	const grounding = $derived(rowGrounding(item));
	const wordLayer = $derived((item.payload as { word_layer?: string }).word_layer ?? null);
	const agent = $derived(item.kind === 'agent_proposal' || item.proposed_by_token != null);
	const tone = $derived(
		quarantined || grounding === 'ungrounded' ? 'bad' : item.reasons.length || lowConf || grounding === 'no_word_layer' ? 'low' : agent ? 'agent' : ''
	);
	// The value as printed (its unit written the usual way), and the stored value in the reader's number
	// format, shown only when it says something the printed one does not.
	const printed = $derived([obs.printed_value, unitLabel(obs.printed_unit == null ? null : String(obs.printed_unit))].filter(Boolean).join(' '));
	const canonical = $derived(
		obs.value_canonical != null && Number.isFinite(Number(obs.value_canonical))
			? `${fmtNum(Number(obs.value_canonical))} ${unitLabel(obs.unit_canonical == null ? null : String(obs.unit_canonical))}`.trim()
			: ''
	);
	const rowText = $derived(Object.values(pl.cells ?? {}).map((c) => c.text ?? '').join(' · '));

	let mode = $state<'' | 'edit' | 'reject' | 'alias' | 'view'>('');
	let reason = $state('');
	let ed = $state({ value_num: '', unit_ucum: '', loinc: '', alias_group: '' });
	let alias = $state({ alias_group: '', loinc: '' });
	let err = $state<{ text: string; raw: string } | null>(null);

	// Run one decision; the form closes only when it went through, else the error shows in this row.
	async function run(p: () => Promise<unknown>, close = true) {
		err = null;
		try {
			await p();
			if (close) mode = '';
		} catch (e) {
			err = { text: errText(e), raw: errRaw(e) };
		}
	}
	function open(m: typeof mode) {
		err = null;
		mode = mode === m ? '' : m;
		if (m === 'edit') ed = { value_num: String(obs.value_num ?? ''), unit_ucum: String(obs.unit_ucum ?? ''), loinc: String(obs.loinc ?? ''), alias_group: String(obs.alias_group ?? '') };
		if (m === 'alias') alias = { alias_group: String(obs.alias_group ?? ''), loinc: String(obs.loinc ?? '') };
		reason = '';
	}
	function saveEdit(e: SubmitEvent) {
		e.preventDefault();
		// '14,8' is how a Spanish reader types it; the server takes '14.8'. Anything else is said here.
		const v = ed.value_num.trim() === '' ? '' : decimalText(ed.value_num);
		if (v == null) {
			err = { text: t('err_decimal'), raw: ed.value_num };
			return;
		}
		const edits: Record<string, unknown> = {};
		for (const [k, raw] of Object.entries({ ...ed, value_num: v })) if (raw !== String(obs[k] ?? '')) edits[k] = raw === '' ? null : raw;
		run(() => onaccept(Object.keys(edits).length ? edits : undefined));
	}
	function checkDecimal(e: Event) {
		const el = e.currentTarget as HTMLInputElement;
		el.setCustomValidity(el.value.trim() && decimalText(el.value) == null ? t('err_decimal') : '');
	}
	// The translated reason is the line; the backend's detail (English, with specifics) is the tooltip,
	// and in English a dim second part. A detail that is only a code never stands in for the reason.
	const label = (c: { reason: string | null }) => {
		const k = 'i2_reason_' + c.reason;
		return t(k) !== k ? t(k) : null;
	};
	const isCode = (d: string) => /^[\w:.-]+$/.test(d.trim()) && !/\s/.test(d.trim());
	type Line = { reason: string | null; detail: string | null; value: string | null };
	const msg = (c: Line) =>
		c.reason === 'conflict' && c.value
			? tf('i2_conflict_val', { v: c.value, date: day(String(obs.effective_at ?? '')) })
			: (label(c) ?? (c.detail && !isCode(c.detail) ? c.detail : t('i2_reason_other')));
	// The server's detail is English: a dim second part in English only, and never one that repeats the label.
	const enLabel = (c: Line) => (dict.en as Record<string, string>)['i2_reason_' + c.reason] ?? null;
	const extra = (c: Line) =>
		i18n.lang === 'en' && c.detail && label(c) && !isCode(c.detail) && !(c.reason === 'conflict' && c.value) && !repeatsLabel(c.detail, enLabel(c)) && !/\([0-9a-f-]{36}\)/.test(c.detail) ? c.detail : '';
	const id = $derived(`rr-${item.id.slice(0, 8)}`);
</script>

<div class="xr {tone}" class:sel={selected} role="group" aria-labelledby="{id}-n">
	<input type="checkbox" {checked} onchange={(e) => oncheck(e.currentTarget.checked)} aria-label="{t('i2_select')} {obs.printed_name}" disabled={quarantined} />
	<div class="c-nm">
		<button class="nm" id="{id}-n" onclick={onselect} aria-pressed={selected} title={t('i2_show_on_page')}>{obs.printed_name}</button>
		<div style="font-size:10.5px" class="dim">{t('i2_range')} {obs.printed_ref_range || '—'}</div>
	</div>
	<div class="mono c-lo" style="font-size:11.5px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis" title="{obs.loinc ?? ''} {obs.alias_group ?? ''}">
		{#if quarantined}—{:else}{obs.loinc ?? t('i2_no_loinc')} <span class="dim">{obs.alias_group}</span>{/if}
	</div>
	<div class="num c-val" style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis" title={quarantined ? undefined : [printed, canonical && canonical !== printed ? canonical : ''].filter(Boolean).join(' → ')}>
		{#if quarantined}{t('i2_free_text')}{:else}{printed}{#if canonical && canonical !== printed}{' '}<span class="dim">→ {canonical}</span>{/if}{/if}
	</div>
	<div class="conf">
		{#if quarantined}
			<span class="pill p-out"><i aria-hidden="true"></i>{t('i2_quarantined')}</span>
		{:else}
			<div class="bar" aria-hidden="true"><i style="width:{Math.max(conf * 100, grounding === 'ungrounded' ? 100 : 0)}%;background:{grounding === 'ungrounded' ? 'var(--out)' : conf > 0.9 ? 'var(--ok)' : 'var(--watch)'}"></i></div>
			<span class="num" aria-label="{t('i2_confidence')} {fmtNum(conf, 2)}">{fmtNum(conf, 2)}</span>
		{/if}
	</div>
	<div class="acts" style="justify-content:flex-end">
		{#if quarantined}
			<button class="btn sm" onclick={() => open('view')} aria-expanded={mode === 'view'}>{t('i2_view')}</button>
			<button class="btn sm no" onclick={() => run(ondiscard, false)} disabled={busy}>{t('i2_discard')}</button>
		{:else}
			<button class="btn sm ok" onclick={() => run(() => onaccept(), false)} disabled={busy} aria-label="{t('i2_accept')} {obs.printed_name}" title={t('i2_accept')}>✓</button>
			<button class="btn sm" onclick={() => open('edit')} aria-expanded={mode === 'edit'}>{t('i2_edit')}</button>
			<button class="btn sm no" onclick={() => open('reject')} aria-expanded={mode === 'reject'} aria-label="{t('i2_reject')} {obs.printed_name}" title={t('i2_reject')}>✕</button>
		{/if}
	</div>
	{#each failed as c, i (i)}
		<div class="why" style="color:{quarantined ? 'var(--out)' : 'var(--watch)'}" title={c.detail ?? undefined}>{quarantined ? '⚠' : '●'} {msg(c)}{#if extra(c)}<span class="dim"> · {extra(c)}</span>{/if}</div>
	{/each}
	{#if grounding === 'ungrounded'}
		<div class="why" style="color:var(--out)" role="note">✕ {t('fu_ungrounded')}</div>
	{:else if grounding === 'no_word_layer'}
		<div class="why" style="color:var(--watch)" role="note">● {t('fu_no_word_layer')}</div>
	{/if}
	{#if agent}
		<div class="why" style="color:var(--accent)">◆ {t('i2_row_agent')}{#if grounding === 'grounded'} · {tf('fu_grounded', { p: Number((item.payload as { page_no?: number }).page_no ?? 1) })}{/if}</div>
		<div class="why"><AgentContext {item} {docName} /></div>
	{:else if wordLayer === 'ocr' || wordLayer === 'paperless_ocr'}
		<div class="why dim">{t(wordLayer === 'ocr' ? 'fu_wl_ocr' : 'fu_wl_paperless')}{#if grounding === 'grounded'} · {tf('fu_grounded', { p: Number((item.payload as { page_no?: number }).page_no ?? 1) })}{/if}</div>
	{/if}
	{#if !failed.length && lowConf && grounding !== 'ungrounded'}
		<div class="why" style="color:var(--watch)">● {tf('i2_low_conf', { c: fmtNum(conf, 2), t: fmtNum(threshold, 2) })}</div>
	{/if}
	{#if unknownName && !quarantined && mode !== 'alias'}
		<div class="why"><button class="lnk" onclick={() => open('alias')}>{t('i2_add_alias')}</button> <span class="dim">{t('i2_add_alias_d')}</span></div>
	{/if}
	{#if mode === 'view'}
		<div class="why"><pre class="p2untrusted">{rowText}</pre><span class="dim" style="font-size:10.5px">trust: untrusted_text</span></div>
	{:else if mode === 'reject'}
		<form class="why p2row" onsubmit={(e) => { e.preventDefault(); run(() => onreject(reason.trim())); }} use:focusOnOpen={{ oncancel: () => (mode = '') }}>
			<input class="p2in" style="flex:1" bind:value={reason} required placeholder={t('i2_reject_reason')} aria-label={t('i2_reject_reason')} />
			<button class="btn sm no" type="submit" disabled={busy}>{t('i2_reject')}</button>
			<button class="btn sm" type="button" onclick={() => (mode = '')}>{t('i2_cancel')}</button>
		</form>
	{:else if mode === 'edit'}
		<form class="why p2row" onsubmit={saveEdit} use:focusOnOpen={{ oncancel: () => (mode = '') }}>
			<input class="p2in num" style="width:80px" inputmode="decimal" bind:value={ed.value_num} oninput={checkDecimal} aria-label={t('i2_value')} placeholder={t('i2_value')} />
			<input class="p2in" style="width:90px" bind:value={ed.unit_ucum} aria-label={t('i2_unit')} placeholder={t('i2_unit')} />
			<input class="p2in mono" style="width:90px" bind:value={ed.loinc} aria-label="LOINC" placeholder="LOINC" />
			<input class="p2in mono" style="width:120px" bind:value={ed.alias_group} aria-label={t('i2_group')} placeholder={t('i2_group')} required />
			<button class="btn sm ok" type="submit" disabled={busy}>{t('i2_save_accept')}</button>
			<button class="btn sm" type="button" onclick={() => (mode = '')}>{t('i2_cancel')}</button>
		</form>
	{:else if mode === 'alias'}
		<form class="why p2row" onsubmit={(e) => { e.preventDefault(); run(() => onalias({ alias_group: alias.alias_group.trim(), loinc: alias.loinc.trim() || null })); }} use:focusOnOpen={{ oncancel: () => (mode = '') }}>
			<span class="dim">“{obs.printed_name}” →</span>
			<input class="p2in mono" style="width:130px" bind:value={alias.alias_group} required aria-label={t('i2_group')} placeholder={t('i2_group')} />
			<input class="p2in mono" style="width:90px" bind:value={alias.loinc} aria-label="LOINC" placeholder="LOINC" />
			<button class="btn sm" type="submit" disabled={busy}>{t('i2_add_alias')}</button>
			<button class="btn sm" type="button" onclick={() => (mode = '')}>{t('i2_cancel')}</button>
		</form>
	{/if}
	{#if err}<div class="why p2err" role="alert" title={err.raw}>{err.text}</div>{/if}
</div>

<style>
	.nm {
		font-weight: 560;
		background: none;
		border: 0;
		padding: 0;
		text-align: left;
		color: var(--text);
		max-width: 100%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.nm:hover {
		color: var(--accent);
	}
	.why {
		grid-column: 2/-1;
		font-size: 11px;
	}
	.sel {
		box-shadow: inset 3px 0 0 var(--accent);
	}
	/* Phone: one block per row. Line 1 the checkbox, the name and the value; line 2 LOINC and the
	   confidence (the range sits under the name); line 3 the actions. */
	@media (max-width: 760px) {
		.xr {
			grid-template-columns: 20px minmax(0, 1fr) minmax(0, 45%);
			grid-template-areas: 'ck nm val' '. lo conf' '. act act';
			row-gap: 4px;
		}
		.xr > :global(input[type='checkbox']) {
			grid-area: ck;
		}
		.xr > .c-nm {
			grid-area: nm;
		}
		.xr > .c-val {
			grid-area: val;
			text-align: right;
		}
		.xr > .c-lo {
			grid-area: lo;
		}
		.xr > .conf {
			grid-area: conf;
			justify-self: end;
		}
		.xr > .acts {
			grid-area: act;
		}
		.why {
			grid-column: 2 / -1;
		}
	}
	.lnk {
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		font-size: 11px;
	}
</style>
