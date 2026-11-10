<script lang="ts">
	// The Inbox screen (v0.2): every configured source with its status, last run and errors; the
	// quarantine of mail from senders not on the allowlist, with "release" and "release and allow
	// sender". Held mail is never read, classified or indexed until a person releases it.
	import '#lib/components/pages2/pages2.css';
	import { tick } from 'svelte';
	import {
		deleteInbox,
		deleteQuarantined,
		issueInboxToken,
		listInboxes,
		listQuarantine,
		releaseQuarantined,
		syncInbox,
		updateInbox,
		type InboxSource,
		type QuarantineOut
	} from '#lib/api/inbox';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { ago, plain } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import SourceCard from '#lib/components/inbox/SourceCard.svelte';
	import SourceForm from '#lib/components/inbox/SourceForm.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { PUSH_KINDS, RECIPE, RECIPES_URL, copyText, heldTotal, quarantineReason } from '#lib/logic/inbox';
	import { app, currentProfile } from '#lib/state/app.svelte';

	const sources = new Loader<InboxSource[]>();
	const held = new Loader<QuarantineOut[]>();
	const load = () => {
		sources.run(async () => (await listInboxes()).items);
		held.run(async () => (await listQuarantine('held')).items);
	};
	$effect(load);

	const profiles = $derived(app.me?.profiles ?? []);
	let form = $state<null | 'new' | InboxSource>(null);
	let busy = $state(false);
	let error = $state<{ text: string; raw: string } | null>(null);
	let notice = $state<{ text: string; tone: 'ok' | 'watch'; lines?: string[] } | null>(null);
	// What the last action on one source card did, shown in that card.
	let card = $state<{ id: string; error?: { text: string; raw: string }; notice?: string } | null>(null);
	let fresh = $state<{ source: InboxSource; token: string } | null>(null);
	// null: not tried yet; false: the clipboard refused, so the person copies the token by hand.
	let copied = $state<boolean | null>(null);
	// Inline release form: which held message, into which profile.
	let releasing = $state<string | null>(null);
	let target = $state('');
	// A held message waiting for "delete for good" (one click alone deletes nothing).
	let deleting = $state<string | null>(null);

	// `on`: the source card the action belongs to; its result shows there instead of at the top.
	async function act(fn: () => Promise<unknown>, done?: string, on?: string) {
		busy = true;
		error = null;
		notice = null;
		card = null;
		try {
			await withStepUp(fn);
			if (done && on) card = { id: on, notice: done };
			else if (done) notice = { text: done, tone: 'ok' };
		} catch (e) {
			const err = { text: errText(e), raw: errRaw(e) };
			if (on) card = { id: on, error: err };
			else error = err;
		} finally {
			busy = false;
			load();
		}
	}
	const token = (s: InboxSource) =>
		act(
			async () => {
				const r = await issueInboxToken(s.id);
				fresh = { source: s, token: r.token };
				copied = null;
			},
			undefined,
			s.id
		);
	const remove = (q: QuarantineOut) =>
		act(async () => {
			await deleteQuarantined(q.id);
			deleting = null;
		});
	function startRelease(q: QuarantineOut) {
		releasing = releasing === q.id ? null : q.id;
		target = currentProfile()?.id ?? profiles[0]?.id ?? '';
	}
	// A problem the server names by code reads in the reader's language; any other shows as sent.
	const problemText = (p: { code: string; message: string }) => (t('fu_qp_' + p.code) !== 'fu_qp_' + p.code ? t('fu_qp_' + p.code) : errText(new Error(p.message)));
	const release = (q: QuarantineOut, allow: boolean) =>
		act(async () => {
			const r = await releaseQuarantined(q.id, { profile_id: target, allow_sender: allow });
			releasing = null;
			const sender = plain(q.sender);
			const problems = r.problems ?? [];
			const n = r.document_ids.length;
			// Nothing came in: say why in the watch tone, never a green "released 0".
			if (!n) {
				notice = { text: tf('fu_q_released_none', { sender }), tone: 'watch', lines: problems.map(problemText) };
				return;
			}
			notice = { text: tp(allow ? 'fu_q_released_allowed' : 'fu_q_released', n, { sender }), tone: problems.length ? 'watch' : 'ok' };
			if (problems.length) notice.lines = [tp('fu_q_released_skipped', problems.length), ...problems.map(problemText)];
		});
	let secretEl: HTMLElement | undefined = $state();
	async function copy() {
		if (!fresh) return;
		copied = await copyText(typeof navigator === 'undefined' ? null : navigator.clipboard, fresh.token, secretEl);
	}
	// Where each push source sends, with which header (connectors.md, ingestion.md 3).
	const PUSH: Record<string, { path: string; header: string }> = {
		paperless: { path: '/api/ingest/paperless', header: 'X-OwnHealth-Secret: ' },
		shortcut: { path: '/api/ingest/shortcut', header: 'Authorization: Bearer ' },
		health_auto_export: { path: '/api/ingest/health-auto-export', header: 'Authorization: Bearer ' }
	};
	const origin = typeof location !== 'undefined' ? location.origin : '';
	const sourceName = (id: string | null) => sources.data?.find((s) => s.id === id)?.label ?? '—';
	const mailboxOf = (id: string | null) => sources.data?.find((s) => s.id === id)?.config ?? null;
	const doneFolder = (id: string | null) => String(sources.data?.find((s) => s.id === id)?.config.done_folder ?? 'OwnHealth/Done');
	const failing = $derived((sources.data ?? []).filter((s) => s.enabled && s.consecutive_failures > 0).length);
	// the server's count, not the length of the first page of the list
	const heldCount = $derived(heldTotal(sources.data));
	const lastSync = $derived(
		(sources.data ?? [])
			.map((s) => s.last_success_at)
			.filter((x): x is string => !!x)
			.sort()
			.at(-1) ?? null
	);
	// The edit form replaced the card, so its Edit button is new: give focus back to it by hand.
	async function closeEdit(id: string) {
		form = null;
		await tick();
		document.querySelector<HTMLElement>(`[data-edit="${id}"]`)?.focus();
	}
	const kb = (n: number) => (n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);
</script>

<PageHeader title={t('fu_inbox')} sub={t('fu_inbox_sub')}>
	{#snippet right()}
		<PhaseChip phase="v0.2" />
		<a class="btn" href="/import-review">{t('nav_imp')}</a>
		<button class="btn pri" onclick={() => (form = form === 'new' ? null : 'new')} aria-expanded={form === 'new'}>{t('fu_add')}</button>
	{/snippet}
</PageHeader>

<div class="inbox">
	<div class="ib"><div><div class="k">{t('fu_sources')}</div><div class="dim d">{t('fu_sources_d')}</div></div><span class="c" class:dim={!sources.data}>{sources.data?.length ?? '—'}</span></div>
	<div class="ib"><div><div class="k">{t('fu_failing')}</div><div class="dim d">{t('fu_failing_d')}</div></div><span class="c" style="color:{failing ? 'var(--out)' : 'var(--dim)'}">{sources.data ? failing : '—'}</span></div>
	<div class="ib"><div><div class="k">{t('fu_quarantine')}</div><div class="dim d">{t('fu_quarantine_d')}</div></div><span class="c" style="color:{heldCount ? 'var(--watch)' : 'var(--dim)'}">{heldCount ?? '—'}</span></div>
	<div class="ib"><div><div class="k">{t('fu_last_ok')}</div><div class="dim d">{t('fu_last_ok_d')}</div></div><span class="c num" class:dim={!lastSync} style="font-size:12.5px;font-weight:560;white-space:nowrap">{!sources.data ? '—' : lastSync ? ago(lastSync) : t('i2_never')}</span></div>
</div>

{#if error}<div class="p2err" role="alert" style="margin-bottom:8px" title={error.raw}>{error.text}</div>{/if}

{#if form === 'new'}
	{#key form}
		<SourceForm
			{profiles}
			source={form === 'new' ? null : form}
			oncancel={() => (form = null)}
			onsaved={(s) => {
				const added = form === 'new';
				form = null;
				load();
				if (added && ['paperless', 'shortcut', 'health_auto_export'].includes(s.kind)) token(s);
			}}
		/>
	{/key}
{/if}

{#if fresh}
	<section class="card tok" aria-live="polite" use:focusOnOpen={{ oncancel: () => (fresh = null) }}>
		<h3>{tf('fu_token_made', { label: fresh.source.label })}<span class="r"><button class="btn sm" onclick={copy}>{copied ? t('i2_copied') : t('i2_copy')}</button> <button class="btn sm" onclick={() => (fresh = null)}>{t('fu_done')}</button></span></h3>
		<div class="p2secret" bind:this={secretEl}>{fresh.token}</div>
		{#if copied === false}<div class="note" role="alert" style="color:var(--watch)">{t('fu_copy_hand')}</div>{/if}
		{#if PUSH[fresh.source.kind]}
			<pre class="code">POST {origin}{PUSH[fresh.source.kind].path}
{PUSH[fresh.source.kind].header}{fresh.token.slice(0, 12)}…</pre>
		{:else}
			<pre class="code">GET {origin}/api/inboxes/{fresh.source.id}/health?format=prometheus
Authorization: Bearer {fresh.token.slice(0, 12)}…</pre>
		{/if}
		<div class="note">
			{t('fu_token_once')} {t(PUSH_KINDS.includes(fresh.source.kind) ? 'fu_tk_' + fresh.source.kind : 'fu_tk_monitor')}
			{#if !PUSH_KINDS.includes(fresh.source.kind)}<a href="https://github.com/GeiserX/OwnHealth/blob/main/deploy/README.md#monitoring-the-inboxes" target="_blank" rel="noopener noreferrer">{t('fu_recipe')} ↗</a>{:else if RECIPE[fresh.source.kind]}<a href={RECIPES_URL + RECIPE[fresh.source.kind]} target="_blank" rel="noopener noreferrer">{t('fu_recipe')} ↗</a>{/if}
		</div>
	</section>
{/if}

{#if sources.error}
	<NotReady error={sources.error} onretry={load} />
{:else if sources.loading && !sources.data}
	<div class="empty">{t('loading')}</div>
{:else if !sources.data?.length}
	<div class="card"><div class="empty">{t('fu_none')} <button class="btn sm pri" onclick={() => (form = 'new')}>{t('fu_add')}</button></div></div>
{:else}
	<div class="srcs">
		{#each sources.data as s (s.id)}
			<!-- The edit form takes the card's own place, so the person keeps the context they clicked in. -->
			{#if form && form !== 'new' && form.id === s.id}
				<div class="editing">
					<SourceForm
						{profiles}
						source={s}
						oncancel={() => closeEdit(s.id)}
						onsaved={() => {
							closeEdit(s.id);
							load();
						}}
					/>
				</div>
			{:else}
				<SourceCard
					source={s}
					{profiles}
					{busy}
					error={card?.id === s.id ? card.error : null}
					notice={card?.id === s.id ? card.notice : null}
					onsync={() => act(() => syncInbox(s.id), tf('fu_sync_queued', { label: s.label }), s.id)}
					ontoggle={(on) => act(() => updateInbox(s.id, { enabled: on }), undefined, s.id)}
					onedit={() => (form = s)}
					ondelete={() => act(() => deleteInbox(s.id))}
					ontoken={() => token(s)}
					onchange={load}
				/>
			{/if}
		{/each}
	</div>
{/if}

<section class="card q" aria-labelledby="q-h">
	<h3 id="q-h">{t('fu_quarantine')}<span class="r">{t('fu_q_r')}</span></h3>
	{#if notice}
		<div class={notice.tone === 'ok' ? 'p2ok' : 'note'} role="status" style="margin-bottom:8px;font-size:12px;{notice.tone === 'watch' ? 'color:var(--watch)' : ''}">
			{notice.text}
			{#each notice.lines ?? [] as l, i (i)}<div>{l}</div>{/each}
		</div>
	{/if}
	{#if held.error}
		<NotReady error={held.error} onretry={load} />
	{:else if held.loading && !held.data}
		<div class="empty">{t('loading')}</div>
	{:else if !held.data?.length}
		<div class="empty">{t('fu_q_none')}</div>
	{:else}
		<div class="tscroll">
			<table class="t stack qt">
				<thead>
					<tr><th>{t('fu_q_received')}</th><th>{t('fu_q_from')}</th><th>{t('fu_q_subject')}</th><th>{t('fu_q_files')}</th><th>{t('fu_q_reason')}</th><th></th></tr>
				</thead>
				<tbody>
					{#each held.data as q (q.id)}
						{@const why = quarantineReason(q.reason, mailboxOf(q.source_id))}
						<tr>
							<td class="dim"><span class="num nw">{ago(q.received_at)}</span><div style="font-size:10.5px">{sourceName(q.source_id)}</div></td>
							<td class="mono ut snd">{plain(q.sender) || '—'}</td>
							<td class="ut"><span title={t('fu_untrusted_title')}>{plain(q.subject) || t('fu_q_nosubject')}</span></td>
							<td data-label={q.attachments.length ? undefined : t('fu_q_files')}>
								{#each q.attachments as a, i (i)}
									<div class="att"><span class="mono">{plain(a.filename)}</span> <span class="dim">{kb(a.size)}</span></div>
								{:else}<span class="dim">{t('fu_q_nofiles')}</span>{/each}
							</td>
							<td class="rsn"><span class="pill {why.tone}" title={why.key === 'fu_qr_auth_unchecked' ? t('fu_q_auth_unchecked_d') : undefined}><i aria-hidden="true"></i>{t(why.key)}</span></td>
							<td class="nw acts-cell" style="text-align:right">
								{#if q.reason !== 'too_large'}
									<button class="btn sm" onclick={() => startRelease(q)} aria-expanded={releasing === q.id}>{t('fu_q_release')}</button>
								{/if}
								<button class="btn sm no" disabled={busy} onclick={() => (deleting = deleting === q.id ? null : q.id)} aria-expanded={deleting === q.id}>{t('fu_delete')}</button>
							</td>
						</tr>
						{#if deleting === q.id}
							<tr class="rel">
								<td colspan="6"><ConfirmRow text={tf('fu_q_delete_d', { sender: plain(q.sender) || '—' })} confirmLabel={t('fu_delete_yes')} {busy} onconfirm={() => remove(q)} oncancel={() => (deleting = null)} /></td>
							</tr>
						{/if}
						{#if q.reason === 'too_large'}
							<tr class="rel">
								<td colspan="6"><div class="note">{tf('fu_q_too_large_d', { folder: doneFolder(q.source_id) })}</div></td>
							</tr>
						{/if}
						{#if releasing === q.id}
							<tr class="rel">
								<td colspan="6">
									<div class="p2row" use:focusOnOpen={{ oncancel: () => (releasing = null) }}>
										<span class="muted" style="font-size:12px">{t('fu_q_into')}</span>
										<select class="p2in" bind:value={target} aria-label={t('fu_f_profile')}>
											{#each profiles as p (p.id)}<option value={p.id}>{p.display_name}</option>{/each}
										</select>
										<!-- The safe choice is the main one: this message only. Allowing the sender reaches every future mail. -->
										<button class="btn sm pri" disabled={busy || !target} onclick={() => release(q, false)}>{t('fu_q_once')}</button>
										<button class="btn sm" disabled={busy || !target || q.reason === 'auth_failed'} onclick={() => release(q, true)}>{t('fu_q_allow')}</button>
										<button class="btn sm" onclick={() => (releasing = null)}>{t('i2_cancel')}</button>
									</div>
									{#if q.reason === 'auth_failed'}<div class="dim" style="font-size:11.5px;margin-top:4px">{t(why.key === 'fu_qr_auth_unchecked' ? 'fu_q_auth_unchecked_d' : 'fu_q_auth_no_allow')}</div>{:else}<div class="note" style="margin-top:4px;color:var(--watch)">{t('fu_q_allow_d')}</div>{/if}
									<div class="note" style="margin-top:4px">{t('fu_q_release_d')}</div>
								</td>
							</tr>
						{/if}
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
	<div class="note" style="margin-top:8px">
		<span class="chip" style="color:var(--out);border-color:rgba(239,107,115,.5)" title={t('fu_untrusted_title')}>{t('fu_untrusted_chip')}</span>
		{t('fu_q_note')}
	</div>
</section>

<style>
	.inbox .k {
		font-weight: 600;
		font-size: 12.5px;
	}
	.inbox .d {
		font-size: 11px;
	}
	.srcs {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(330px, 1fr));
		/* An open card takes a whole row; the cards after it fill the gap it leaves. */
		grid-auto-flow: row dense;
		gap: 12px;
		margin-bottom: 12px;
	}
	.editing {
		grid-column: 1 / -1;
	}
	.editing :global(.sf) {
		margin-bottom: 0;
	}
	.tok {
		border-color: rgba(224, 168, 74, 0.55);
		margin-bottom: 12px;
		display: grid;
		gap: 8px;
	}
	.q .ut {
		max-width: 320px;
		color: var(--muted);
	}
	.q .snd {
		word-break: break-all;
		min-width: 14ch;
	}
	.att {
		font-size: 11.5px;
		white-space: nowrap;
	}
	.nw {
		white-space: nowrap;
	}
	tr.rel td {
		background: var(--raised);
	}
	.inbox {
		flex-wrap: wrap;
	}
	.inbox .ib {
		flex: 1 1 170px;
		min-width: 0;
	}
	/* Tablet: the table may scroll sideways, but the buttons, the only actions on a row, stay in view
	   at its right edge, and the sender gets room for a readable line. */
	@media (max-width: 1100px) {
		.qt td.acts-cell,
		.qt th:last-child {
			position: sticky;
			right: 0;
			background: var(--surface);
			box-shadow: -8px 0 8px -8px rgba(0, 0, 0, 0.6);
		}
		.q .snd {
			min-width: 22ch;
		}
	}
	@media (max-width: 760px) {
		.qt td.acts-cell,
		.qt th:last-child {
			position: static;
			background: none;
			box-shadow: none;
		}
		/* A long value ('hace 10 minutos') goes under the label instead of past the tile's edge. */
		.inbox .ib {
			flex-wrap: wrap;
		}
		.inbox .ib > div {
			flex: 1 1 auto;
			min-width: 0;
		}
		.srcs {
			grid-template-columns: minmax(0, 1fr);
		}
		/* Each held message is a block: when and from where, the sender on its own full line, the subject,
		   the files and the reason, then the buttons. */
		.qt td.snd {
			display: block;
			width: 100%;
			max-width: none;
			margin: 0;
			color: var(--text);
			font-size: 12px;
		}
		.qt td.ut:not(.snd) {
			display: block;
			width: 100%;
			max-width: none;
			margin: 0;
		}
		.qt td.rsn {
			display: block;
			width: auto;
			margin: 2px 0 0;
		}
		.qt .att {
			white-space: normal;
		}
	}
</style>
