<script lang="ts">
	// The Connections screen (v1.0, slice 55): every health record and device connection of the
	// profiles this person manages, with its status, last sync, when its sign-in and access token end,
	// open sync-gap alerts, and Reconnect. The connector callback lands here with ?connected=<id> or
	// ?connector_error=<code>. The add wizard (slice 19) launches new connections.
	import '#lib/components/pages2/pages2.css';
	import { tick } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { ApiError } from '#lib/api/core';
	import { disconnectConnector, listSourceHealth, reconnectConnector, type DisconnectOut } from '#lib/api/connections';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { ago, day } from '#lib/components/pages2/fmt';
	import { dict } from '#lib/i18n/dict';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { intlLocale } from '#lib/i18n/langs';
	import { afterColon } from '#lib/logic/format';
	import {
		callbackOutcome,
		canReconnect,
		connectionState,
		connectorErrorKey,
		daysLeft,
		refreshUntilFirstPull,
		renewsItself,
		runWords,
		shownConnections,
		statusWord,
		wantsReconnect,
		type ConnectionRow,
		type ConnectionState
	} from '#lib/logic/connections';
	import { app, setProfile } from '#lib/state/app.svelte';

	// One clock for every date on the screen: it moves on each minute and on each load, so a grant that
	// ends while the page is open reads ended, and an access token past its time reads none.
	let now = $state(new Date());
	$effect(() => {
		const timer = setInterval(() => (now = new Date()), 60_000);
		return () => clearInterval(timer);
	});

	const rows = new Loader<ConnectionRow[]>();
	const load = () =>
		rows.run(async () => {
			const list = shownConnections(await listSourceHealth());
			now = new Date();
			return list;
		});
	$effect(() => {
		load();
	});

	const has = (k: string) => k in dict.en;
	const codeText = (code: string) => tf(connectorErrorKey(code, has), { code });
	// a poller's stop or problem, kept in English by the server, in the reader's language when its code has words
	const runText = (text: string) => runWords(text, has, tf);

	// What the vendor's redirect said, read once; the address is then cleaned so a reload does not repeat it.
	let outcome = $state<{ connected: string } | { error: string } | null>(null);
	$effect(() => {
		const o = callbackOutcome(page.url.searchParams);
		if (!o) return;
		outcome = o;
		goto('/connections', { replace: true, shallow: true });
	});
	const connectedId = $derived(outcome && 'connected' in outcome ? outcome.connected : null);
	// the first pull runs seconds after a connect or reconnect: the rows are read again until it finishes, so the card shows it
	$effect(() => {
		const id = connectedId;
		if (!id) return;
		let alive = true;
		let timer: ReturnType<typeof setTimeout> | undefined;
		const wait = (ms: number) => new Promise<void>((done) => (timer = setTimeout(done, ms)));
		refreshUntilFirstPull(
			async () => {
				await load();
				return rows.data;
			},
			id,
			wait,
			() => alive
		);
		return () => {
			alive = false;
			clearTimeout(timer);
		};
	});
	// the note shows only for a connection this person sees listed, never for any id in the address
	const connectedLabel = $derived(connectedId ? (rows.data?.find((r) => r.id === connectedId)?.label ?? null) : null);

	const states = $derived(new Map((rows.data ?? []).map((r) => [r.id, connectionState(r, now)] as const)));
	const attention = $derived([...states.values()].filter(wantsReconnect).length);
	const lastSync = $derived(
		(rows.data ?? [])
			.map((r) => r.last_success_at)
			.filter((x): x is string => !!x)
			.sort()
			.at(-1) ?? null
	);
	// Open in review shows the queue of the connection's own profile, so the alert is there.
	const reviewOf = (r: ConnectionRow) => {
		const slug = app.me?.profiles.find((p) => p.id === r.profile_id)?.slug;
		if (slug) setProfile(slug);
	};
	const profileName = (id: string | null) => (id ? (app.me?.profiles.find((p) => p.id === id)?.display_name ?? '—') : t('con_household'));
	const time = (iso: string) => new Date(iso).toLocaleTimeString(intlLocale(i18n.lang), { hour: '2-digit', minute: '2-digit' });

	const PILL = { ok: 'p-ok', watch: 'p-watch', out: 'p-out', stale: 'p-stale', nodata: 'p-stale', off: 'p-stale' } as const;
	const WORD = { ok: 'fu_st_ok', watch: 'watch', out: 'fu_st_out', stale: 'fu_st_stale', nodata: 'fu_st_nodata', off: 'fu_st_off' } as const;

	let busy = $state<string | null>(null);
	let confirming = $state<string | null>(null);
	// What the last action on one card did, shown in that card; above the list when the card is gone.
	let card = $state<{ id: string; label: string; error?: { text: string; raw: string }; done?: DisconnectOut } | null>(null);
	const orphan = $derived(card?.error && rows.data && !rows.data.some((r) => r.id === card?.id) ? card : null);

	const failure = (e: unknown) => ({ text: e instanceof ApiError && has('con_err_' + e.code) ? codeText(e.code) : errText(e), raw: errRaw(e) });

	async function reconnect(r: ConnectionRow) {
		busy = r.id;
		card = null;
		try {
			const { authorize_url } = await reconnectConnector(r.id);
			// the vendor's sign-in; its callback brings the browser back here
			location.assign(authorize_url);
		} catch (e) {
			card = { id: r.id, label: r.label, error: failure(e) };
			busy = null;
		}
	}

	async function disconnect(r: ConnectionRow) {
		busy = r.id;
		card = null;
		try {
			const done = await disconnectConnector(r.id);
			card = { id: r.id, label: r.label, done };
			confirming = null;
		} catch (e) {
			card = { id: r.id, label: r.label, error: failure(e) };
		} finally {
			busy = null;
		}
		await load();
		// the Disconnect button is gone: keyboard focus goes to the card's name, or to the message above
		// the list when the card is gone too
		await tick();
		(document.getElementById('con-' + r.id) ?? document.getElementById('con-orphan'))?.focus();
	}

	async function dismissOutcome() {
		outcome = null;
		await tick();
		const h = document.querySelector<HTMLElement>('h1');
		if (h) {
			h.tabIndex = -1;
			h.focus();
		}
	}

	const revocationText = (r: ConnectionRow, done: DisconnectOut) =>
		(done.revocation === 'failed' || done.revocation === 'not_offered') && !done.revoke_page && r.server
			? tf('con_rev_' + done.revocation + '_at', { label: r.label, server: r.server })
			: tf('con_rev_' + done.revocation, { label: r.label });

	const stateLine = (r: ConnectionRow, s: ConnectionState): { text: string; tone: string } | null => {
		const n = daysLeft(r.grant_expires_at, now) ?? 0;
		switch (s) {
			case 'reconsent':
				return { text: t('con_st_reconsent'), tone: 'var(--out)' };
			case 'expired':
				return { text: tf('con_st_expired', { date: day(r.grant_expires_at) }), tone: 'var(--out)' };
			case 'grant_soon':
				return { text: tp('con_st_grant_soon', n, { date: day(r.grant_expires_at) }), tone: 'var(--watch)' };
			case 'off':
				return canReconnect(r) ? { text: t('con_st_off'), tone: 'var(--dim)' } : null;
			case 'stale':
				return { text: t('fu_stale_d'), tone: 'var(--stale)' };
			default:
				return null;
		}
	};
</script>

<PageHeader title={t('con_nav')} sub={t('con_sub')}>
	{#snippet right()}
		<PhaseChip phase="v1.0" />
		<a class="btn" href="/inbox">{t('fu_inbox')}</a>
	{/snippet}
</PageHeader>

<div class="inbox tiles">
	<div class="ib"><div><div class="k">{t('con_count')}</div><div class="dim d">{t('con_count_d')}</div></div><span class="c" class:dim={!rows.data}>{rows.data?.length ?? '—'}</span></div>
	<div class="ib"><div><div class="k">{t('con_attention')}</div><div class="dim d">{t('con_attention_d')}</div></div><span class="c" style="color:{attention ? 'var(--out)' : 'var(--dim)'}">{rows.data ? attention : '—'}</span></div>
	<div class="ib"><div><div class="k">{t('fu_last_ok')}</div><div class="dim d">{t('con_last_ok_d')}</div></div><span class="c num" class:dim={!lastSync} style="font-size:12.5px;font-weight:560;white-space:nowrap">{!rows.data ? '—' : lastSync ? ago(lastSync) : t('i2_never')}</span></div>
</div>

{#if outcome && 'error' in outcome}
	<div class="p2err" role="alert" style="margin-bottom:8px" data-code={outcome.error}>
		{codeText(outcome.error)}
		<button class="btn sm" style="margin-left:8px" onclick={dismissOutcome}>{t('fu_done')}</button>
	</div>
{:else if connectedLabel}
	<div class="p2ok" role="status" style="margin-bottom:8px">{tf('con_connected', { label: connectedLabel })}</div>
{/if}
{#if orphan?.error}
	<div class="p2err" role="alert" id="con-orphan" tabindex="-1" style="margin-bottom:8px" title={orphan.error.raw}>{orphan.label}: {orphan.error.text}</div>
{/if}

{#if rows.error}
	<NotReady error={rows.error} onretry={load} />
{:else if rows.loading && !rows.data}
	<div class="empty">{t('loading')}</div>
{:else if !rows.data?.length}
	<div class="card"><div class="empty">{t('con_none')}</div></div>
{:else}
	<div class="conns">
		{#each rows.data as r (r.id)}
			{@const s = states.get(r.id) ?? 'nodata'}
			{@const word = statusWord(s)}
			{@const line = stateLine(r, s)}
			{@const left = daysLeft(r.grant_expires_at, now)}
			{@const lastErr = r.last_error ? (runText(r.last_error) ?? errText(new Error(r.last_error))) : ''}
			{@const problem = r.last_run_problem ? runText(r.last_run_problem) : null}
			<article class="card conn" class:just={connectedId === r.id} aria-labelledby="con-{r.id}" data-state={s}>
				<div class="hd">
					<div style="min-width:0">
						<b id="con-{r.id}" tabindex="-1">{r.label}</b>
						<div class="dim kind">{t('con_k_' + r.kind)}</div>
					</div>
					<span class="pill {PILL[word]}"><i aria-hidden="true"></i>{t(WORD[word])}</span>
				</div>
				<dl class="facts">
					<dt>{t('fu_to')}</dt>
					<dd>{profileName(r.profile_id)}</dd>
					<dt>{t('fu_last_ok')}</dt>
					<dd class:stale={s === 'stale'}><span class="num nw">{r.last_success_at ? ago(r.last_success_at) : t('i2_never')}</span></dd>
					<dt>{t('fu_last_run')}</dt>
					<dd><span class="num nw">{r.last_run_at ? ago(r.last_run_at) : t('i2_never')}</span></dd>
					{#if r.server}
						<dt>{t('con_server')}</dt>
						<dd class="mono srv">{r.server}</dd>
					{/if}
					{#if r.enabled && (canReconnect(r) || r.grant_expires_at)}
						<dt>{t('con_grant')}</dt>
						<dd class="grant" style={s === 'expired' ? 'color:var(--out)' : s === 'grant_soon' ? 'color:var(--watch)' : ''}>
							{#if r.grant_expires_at && left != null && left > 0}{tp('con_left', left, { date: day(r.grant_expires_at) })}{:else if r.grant_expires_at}{day(r.grant_expires_at)}{:else}<span class="dim">{t('con_grant_none')}</span>{/if}
						</dd>
						<dt>{t('con_access')}</dt>
						<dd>{#if r.access_expires_at && new Date(r.access_expires_at) > now}{tf(renewsItself(s) ? 'con_access_until' : 'con_access_until_kept', { time: time(r.access_expires_at) })}{:else}<span class="dim">{t('con_access_none')}</span>{/if}</dd>
					{/if}
				</dl>
				{#if line}<div class="note state" role="status" style="color:{line.tone}">{line.text}</div>{/if}
				{#if s === 'out' && r.last_error}
					<div class="err" role="status">⚠ {tp('fu_failed_n', r.consecutive_failures)} {afterColon(lastErr, i18n.lang)}</div>
				{:else if s === 'partial'}
					<div class="note" role="status" style="color:var(--watch);overflow-wrap:anywhere">⚠ {tp('fu_partial_n', r.last_run_problems ?? 0)} {#if problem}{problem}{:else if r.last_run_problem}<span class="mono">{r.last_run_problem}</span>{/if}</div>
				{/if}
				{#if r.open_gaps}
					<div class="note gaps" role="status">{tp('con_gaps', r.open_gaps)} <a href="/import-review" onclick={() => reviewOf(r)}>{t('con_gaps_go')} →</a></div>
				{/if}
				{#if canReconnect(r)}
					<div class="acts">
						<button class="btn sm" class:pri={wantsReconnect(s) || s === 'off'} disabled={busy != null} onclick={() => reconnect(r)}>{t('con_reconnect')}</button>
						{#if r.enabled}
							<button class="btn sm no del" disabled={busy != null} aria-expanded={confirming === r.id} onclick={() => (confirming = confirming === r.id ? null : r.id)}>{t('con_disconnect')}</button>
						{/if}
					</div>
				{/if}
				{#if confirming === r.id}
					<ConfirmRow text={tf('con_disconnect_d', { label: r.label })} confirmLabel={t('con_disconnect_yes')} busy={busy === r.id} onconfirm={() => disconnect(r)} oncancel={() => (confirming = null)} />
				{/if}
				{#if card?.id === r.id && card.error}<div class="p2err" role="alert" title={card.error.raw}>{card.error.text}</div>{/if}
				{#if card?.id === r.id && card.done}
					<div class={card.done.revocation === 'revoked' || card.done.revocation === 'no_token' ? 'p2ok' : 'note'} role="status" style="font-size:12px;{card.done.revocation === 'failed' || card.done.revocation === 'not_offered' ? 'color:var(--watch)' : ''}">
						{revocationText(r, card.done)}
						{#if card.done.revoke_page}<a href={card.done.revoke_page} target="_blank" rel="noopener noreferrer">{t('con_revoke_page')} ↗</a>{/if}
					</div>
				{/if}
			</article>
		{/each}
	</div>
{/if}

<style>
	.tiles {
		flex-wrap: wrap;
	}
	.tiles .ib {
		flex: 1 1 170px;
		min-width: 0;
	}
	.conns {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(330px, 1fr));
		gap: 12px;
		margin-bottom: 12px;
	}
	.conn {
		display: flex;
		flex-direction: column;
		gap: 8px;
		min-width: 0;
	}
	.conn.just {
		border-color: var(--ok);
	}
	.hd {
		display: flex;
		gap: 10px;
		align-items: flex-start;
	}
	.hd .pill {
		margin-left: auto;
	}
	.kind {
		font-size: 11.5px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.facts {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 2px 12px;
		margin: 0;
		font-size: 12px;
	}
	.facts dt {
		color: var(--dim);
	}
	.facts dd {
		margin: 0;
		min-width: 0;
	}
	.facts dd.stale {
		color: var(--stale);
	}
	.srv {
		font-size: 11px;
		overflow-wrap: anywhere;
	}
	.nw {
		white-space: nowrap;
	}
	.state,
	.gaps {
		font-size: 12px;
	}
	.gaps {
		color: var(--watch);
	}
	.err {
		font-size: 11.5px;
		color: var(--out);
		overflow-wrap: anywhere;
	}
	.acts {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		margin-top: auto;
	}
	.acts .del {
		margin-left: auto;
	}
	@media (max-width: 760px) {
		.conns {
			grid-template-columns: minmax(0, 1fr);
		}
		.tiles .ib {
			flex-wrap: wrap;
		}
		.tiles .ib > div {
			flex: 1 1 auto;
			min-width: 0;
		}
	}
</style>
