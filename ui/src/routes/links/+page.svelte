<script lang="ts">
	// Share links: the owner or a guardian sends the profile's summary to a doctor as a SMART Health
	// Link (backend routes/shl.rs). New link: what it carries, expiry, an optional PIN, the language.
	// The link and its QR code show once, right after it is made, and live only in this page's memory:
	// the server keeps no copy of the key. A link answered after the person moved to another profile
	// waits in that memory and shows once when its own profile is on screen again, never on another.
	// Each link says its state, who opened it, and can be revoked.
	// The PIN input has no name, so no form submit can ever put it in a URL; it is cleared once used.
	// What the form offers is what the server will accept: the list answer's `open` categories.
	import { tick } from 'svelte';
	import '#lib/components/pages2/pages2.css';
	import { ApiError } from '#lib/api/core';
	import type { ShlAccess, ShlCreateIn, ShlCreated, ShlLink, ShlList } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import { ago, day, stamp } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { LANGS, type Lang } from '#lib/i18n/langs';
	import { copyText } from '#lib/logic/inbox';
	import { currentProfile } from '#lib/state/app.svelte';
	import { createLink, linkAccess, listLinks, revokeLink } from './api';
	import { DAYS, DEFAULT_DAYS, PICKS, accessKey, canShare, categoriesFor, expiryKey, mayShare, pinProblem, revocable, shownCategories, stateOf } from './logic';
	import { qrOf } from './qr';

	const prof = $derived(currentProfile());
	const slug = $derived(prof?.slug ?? '');
	const name = $derived(prof?.display_name ?? '');
	const sharer = $derived(mayShare(prof?.role, prof?.break_glass));

	const links = new Loader<ShlList>();
	// what a new link may carry, from the server (routes/shl.rs open_categories)
	const open = $derived((links.data?.open ?? []) as string[]);
	// the server refuses this person on this profile even when /me looked fine (a grant changed since)
	const refused = $derived(links.error instanceof ApiError && links.error.code === 'not_sharer');
	const log = new Loader<ShlAccess[]>();
	let label = $state('');
	let picked = $state<string[]>([...PICKS]);
	let days = $state(DEFAULT_DAYS);
	let pin = $state('');
	let lang = $state<Lang>(i18n.lang);
	let busy = $state(false);
	let error = $state<{ text: string; raw: string } | null>(null);
	type Made = { id: string; label: string | null; shlink: string; pin: boolean; leftOut: number | null; qr: { size: number; path: string } };
	// The link just made: in this page's memory only, gone on reload or on another profile.
	let fresh = $state<Made | null>(null);
	// Links answered while another profile was on screen, by the profile they were made for. Never in
	// storage; each one leaves this map the moment it shows.
	const waiting = new Map<string, Made>();
	// Moves on every profile switch: an answer knows whether the person is still in the visit that asked.
	let visit = 0;
	let copied = $state<boolean | null>(null);
	// One clock for the rows: a link whose expiry passes while the page is open reads expired.
	let now = $state(Date.now());
	$effect(() => {
		const timer = setInterval(() => (now = Date.now()), 60_000);
		return () => clearInterval(timer);
	});
	let linkEl: HTMLElement | undefined = $state();
	let madeEl: HTMLElement | undefined = $state();
	let confirming = $state<string | null>(null);
	let logOf = $state<string | null>(null);
	let rowError = $state<{ id: string; text: string; raw: string } | null>(null);

	const load = () =>
		links.run(async () => {
			const list = await listLinks(slug);
			now = Date.now();
			return list;
		}, slug);
	// another profile: nothing typed or made for the last one stays on screen; a link made for this
	// one while it was away shows now, once
	$effect(() => {
		const s = slug;
		const ok = sharer;
		visit++;
		fresh = null;
		const kept = ok ? waiting.get(s) : undefined;
		if (kept) {
			fresh = kept;
			waiting.delete(s);
		}
		copied = null;
		confirming = null;
		logOf = null;
		error = null;
		label = '';
		pin = '';
		picked = [...PICKS];
		if (s && ok) load();
	});
	const pinWrong = $derived(pinProblem(pin));

	async function make() {
		// the profile this link is for, and the visit that asked: an answer that lands later belongs to them
		const s = slug;
		const v = visit;
		// what the person confirmed by pressing Create: a step-up retry sends this same body
		const body: ShlCreateIn = { categories: categoriesFor(picked, open), days, passcode: pin || undefined, label: label.trim() || undefined, lang };
		busy = true;
		error = null;
		try {
			const made: ShlCreated = await withStepUp(() => createLink(s, body));
			// the server has the link now, and this is the only copy of its key: never drop it
			const m: Made = {
				id: made.link.id,
				label: made.link.label,
				shlink: made.shlink,
				pin: made.link.passcode,
				leftOut: made.left_out?.sensitive ?? null,
				qr: qrOf(made.shlink)
			};
			if (visit !== v) {
				// the person moved: show it on its own profile, leave any new draft alone
				if (slug === s && sharer) {
					fresh = m;
					copied = null;
					load();
				} else waiting.set(s, m);
				return;
			}
			fresh = m;
			copied = null;
			pin = '';
			label = '';
			load();
			await tick();
			madeEl?.focus();
		} catch (e) {
			if (visit !== v) return;
			error = { text: errText(e), raw: errRaw(e) };
			// a lock or a grant changed since the page loaded: ask again what may be shared
			if (e instanceof ApiError && (e.code === 'category_closed' || e.code === 'summary_closed')) load();
		} finally {
			busy = false;
		}
	}
	async function revoke(x: ShlLink) {
		const s = slug;
		busy = true;
		rowError = null;
		try {
			await revokeLink(s, x.id);
			if (slug !== s) return;
			confirming = null;
			if (fresh?.id === x.id) fresh = null;
			load();
			if (logOf === x.id) openLog(x.id);
			// the Revoke button is gone: keep the keyboard on the same row
			await tick();
			document.getElementById(`shl-log-${x.id}`)?.focus();
		} catch (e) {
			if (slug === s) rowError = { id: x.id, text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}
	function openLog(id: string) {
		logOf = id;
		log.run(async () => (await linkAccess(slug, id)).items, id);
	}
	async function copy() {
		const f = fresh;
		if (!f) return;
		const ok = await copyText(typeof navigator === 'undefined' ? null : navigator.clipboard, f.shlink, linkEl);
		// the feedback is for the link that was copied: another one may be on screen by now
		if (fresh?.id === f.id) copied = ok;
	}
	const title = (x: ShlLink) => x.ice ? t('ice_title') : x.label ?? t('shl_nolabel');
	const madeTitle = (x: { label: string | null }) => (x.label ? tf('shl_made', { label: x.label }) : t('shl_made0'));
	const cat = (c: string) => t('shl_c_' + c.replace('read:', ''));
	// One line of what a link carries, one of its dates and use.
	function carries(x: ShlLink): string {
		if (x.ice) return t(x.online ? 'ice_link_online' : 'ice_link_offline');
		const parts = shownCategories(x.categories).map((c) => t('shl_s_' + c.replace('read:', '')));
		if (x.passcode) parts.push(x.attempts_left !== null && revocable(x, now) ? `${t('shl_pin_on')} (${tp('shl_tries', x.attempts_left)})` : t('shl_pin_on'));
		return parts.join(' · ');
	}
	function when(x: ShlLink): string {
		const parts = [`${t('shl_created')} ${day(x.created_at)}`, `${t(expiryKey(x, now))} ${day(x.expires_at)}`];
		if (!x.ice || x.online) parts.push(x.opened ? tp('shl_opened', x.opened) : t('shl_opened0'));
		if (x.last_access_at) parts.push(`${t('shl_last')} ${ago(x.last_access_at)}`);
		return parts.join(' · ');
	}
</script>

<PageHeader title={t('shl_nav')} sub={name && sharer && !refused ? tf('shl_sub', { name }) : undefined} />

{#if !prof}
	<div class="card"><div class="empty" role="status">{t('fu_oa_noprofiles')}</div></div>
{:else if prof.break_glass}
	<div class="card"><div class="empty" role="status">{tf('shl_who_bg', { name })}</div></div>
{:else if !sharer || refused}
	<div class="card"><div class="empty" role="status">{tf('shl_who_no', { name })}</div></div>
{:else}
	<div class="sl-wrap">
		<!-- in the page from the start, so a screen reader announces the new link when it appears -->
		<div class="sr-only" aria-live="polite">{fresh ? madeTitle(fresh) : ''}</div>
		{#if links.data && !canShare(open)}
			<div class="card"><div class="empty" role="status">{tf('shl_summary_closed', { name })}</div></div>
		{:else if links.data}
		<section class="card p2form" aria-labelledby="shl-new">
			<div class="sl-head">
				<h3 id="shl-new">{t('shl_new')}</h3>
				<span class="dim sl-small" id="shl-new-d">{t('i2_tok_stepup')}</span>
			</div>
			<label
				>{t('shl_label')}
				<input class="p2in" maxlength="80" autocomplete="off" placeholder={t('shl_label_ph')} bind:value={label} />
			</label>
			<fieldset>
				<legend>{t('shl_what')}</legend>
				<label class="sl-chk"><input type="checkbox" checked disabled />{t('shl_c_summary')}</label>
				{#each PICKS as c (c)}
					{#if open.includes(c)}
						<label class="sl-chk"><input type="checkbox" value={c} bind:group={picked} />{cat(c)}</label>
					{:else}
						<!-- closed for this person on this profile: shown, never ticked, never sent -->
						<label class="sl-chk dim"><input type="checkbox" disabled />{cat(c)}<span class="dim">· {t('shl_c_closed')}</span></label>
					{/if}
				{/each}
				<div class="note">{t('shl_never')}</div>
			</fieldset>
			<div class="sl-two">
				<label
					>{t('shl_days')}
					<select class="p2in" bind:value={days}>
						{#each DAYS as d (d)}<option value={d}>{tp('shl_days_n', d)}</option>{/each}
					</select>
				</label>
				<label
					>{t('shl_lang')}
					<select class="p2in" bind:value={lang}>
						{#each LANGS as l (l.code)}<option value={l.code}>{l.name}</option>{/each}
					</select>
				</label>
			</div>
			<label
				>{t('shl_pin')}
				<!-- no maxlength: it counts UTF-16 units, the server counts characters (pinProblem) -->
				<input class="p2in" type="password" autocomplete="off" spellcheck="false" aria-invalid={!!pinWrong} aria-describedby="shl-pin-d" bind:value={pin} />
			</label>
			<div class="note" id="shl-pin-d">{pinWrong ? t(pinWrong) : t('shl_pin_d')}</div>
			{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
			<div class="p2row">
				<button class="btn pri" disabled={busy || !!pinWrong} aria-busy={busy} aria-describedby="shl-new-d" onclick={make}>{busy ? t('shl_making') : t('shl_make')}</button>
			</div>
		</section>
		{/if}

		{#if fresh}
			<section class="card sl-fresh" aria-labelledby="shl-made">
				<h3 id="shl-made" tabindex="-1" bind:this={madeEl}>{madeTitle(fresh)}</h3>
				<div class="sl-made">
					<svg class="sl-qr" viewBox="0 0 {fresh.qr.size} {fresh.qr.size}" role="img" aria-label={t('shl_qr')} shape-rendering="crispEdges">
						<rect width={fresh.qr.size} height={fresh.qr.size} fill="#fff" />
						<path d={fresh.qr.path} fill="#000" />
					</svg>
					<div class="sl-side">
						<div class="p2secret sl-url" bind:this={linkEl} data-testid="shl-link">{fresh.shlink}</div>
						<div class="p2row"><button class="btn sm" onclick={copy}>{copied ? t('i2_copied') : t('i2_copy')}</button></div>
						{#if copied === false}<div class="note" role="alert" style="color:var(--watch)">{t('shl_copy_hand')}</div>{/if}
						<div class="note" style="color:var(--watch)">{t('shl_once')}</div>
						{#if fresh.pin}<div class="note">{t('shl_pin_apart')}</div>{/if}
						{#if fresh.leftOut !== null && fresh.leftOut > 0}<div class="note">{tp('shl_left_out', fresh.leftOut)}</div>{/if}
						<div class="note">{t('shl_viewer')}</div>
						<div class="p2row"><button class="btn sm" onclick={() => (fresh = null)}>{t('shl_hide')}</button></div>
					</div>
				</div>
			</section>
		{/if}

		<section class="card" aria-labelledby="shl-list">
			<h3 id="shl-list">{tf('shl_list', { name })}</h3>
			{#if links.error}
				<NotReady error={links.error} onretry={load} />
			{:else if !links.data}
				<div class="empty">{t('loading')}</div>
			{:else}
				<ul class="sl-list">
					{#each links.data.items as x (x.id)}
						{@const st = stateOf(x, now)}
						<li data-testid="shl-row">
							<div class="sl-row">
								<div class="sl-main">
									<div class="sl-top">
										<b>{title(x)}</b>
										<span class="pill {st.tone}"><i aria-hidden="true"></i>{t(st.key)}</span>
										{#if st.why}<span class="dim sl-small">{t(st.why)}</span>{/if}
									</div>
									<div class="dim sl-small">{carries(x)}</div>
									<div class="dim sl-small">{when(x)}</div>
								</div>
								<div class="sl-acts">
									{#if !x.ice || x.online}<button class="btn sm" id="shl-log-{x.id}" aria-expanded={logOf === x.id} aria-label="{t('shl_log')} · {title(x)}" onclick={() => (logOf === x.id ? (logOf = null) : openLog(x.id))}>{t('shl_log')}</button>{/if}
									{#if revocable(x, now)}
										<button class="btn sm no" disabled={busy} aria-expanded={confirming === x.id} aria-label="{t('i2_revoke')} · {title(x)}" onclick={() => (confirming = x.id)}>{t('i2_revoke')}</button>
									{/if}
								</div>
							</div>
							{#if confirming === x.id}
								<ConfirmRow text={t(x.ice ? 'ice_revoke_d' : 'shl_revoke_d')} confirmLabel={t('i2_revoke')} {busy} onconfirm={() => revoke(x)} oncancel={() => (confirming = null)} />
							{/if}
							{#if rowError?.id === x.id}<div class="p2err" role="alert" title={rowError.raw}>{rowError.text}</div>{/if}
							{#if logOf === x.id}
								<div class="sl-log">
									{#if log.error}
										<NotReady error={log.error} onretry={() => openLog(x.id)} />
									{:else if !log.data}
										<div class="empty">{t('loading')}</div>
									{:else if !log.data.length}
										<div class="dim sl-small">{t('shl_log_none')}</div>
									{:else}
										<div class="tscroll">
											<table class="t" style="font-size:12px">
												<thead><tr><th>{t('shl_at')}</th><th>{t('shl_event')}</th><th>{t('shl_recipient')}</th><th>{t('shl_ip')}</th><th>{t('shl_ua')}</th></tr></thead>
												<tbody>
													{#each log.data as a, i (i)}
														<tr>
															<td class="num" style="white-space:nowrap">{stamp(a.at)}</td>
															<td>{t(accessKey(a.kind))}</td>
															<td>{a.recipient}</td>
															<td class="mono">{a.ip ?? '—'}</td>
															<td class="dim sl-ua" title={a.user_agent ?? ''}>{a.user_agent ?? '—'}</td>
														</tr>
													{/each}
												</tbody>
											</table>
										</div>
										<div class="note">{t('shl_recipient_d')}</div>
									{/if}
								</div>
							{/if}
						</li>
					{:else}
						<li class="dim sl-small">{t('shl_none')}</li>
					{/each}
				</ul>
			{/if}
		</section>
	</div>
{/if}

<StepUpDialog />

<style>
	.sl-wrap {
		max-width: 760px;
		display: grid;
		gap: 12px;
	}
	fieldset {
		border: 0;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	legend {
		font-size: 11.5px;
		color: var(--muted);
		margin-bottom: 3px;
		padding: 0;
	}
	.sl-chk {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 12.5px;
		color: var(--text);
	}
	.sl-head {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 8px;
		margin-bottom: 8px;
	}
	.sl-head h3 {
		margin: 0;
	}
	.sl-two {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 8px;
	}
	.sl-made {
		display: flex;
		gap: 14px;
		align-items: flex-start;
		flex-wrap: wrap;
	}
	.sl-qr {
		width: 220px;
		height: 220px;
		border-radius: 6px;
		flex: none;
	}
	.sl-side {
		flex: 1;
		min-width: 240px;
		display: grid;
		gap: 6px;
	}
	.sl-url {
		overflow-wrap: anywhere;
		font-size: 11px;
		max-height: 7.5em;
		overflow: auto;
	}
	.sl-fresh {
		border-color: rgba(224, 168, 74, 0.55);
	}
	.sl-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
	}
	.sl-list > li {
		padding: 8px 2px;
		border-bottom: 1px solid var(--line);
	}
	.sl-list > li:last-child {
		border-bottom: 0;
	}
	.sl-row {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		flex-wrap: wrap;
	}
	.sl-main {
		flex: 1;
		min-width: 0;
	}
	.sl-top {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
	}
	.sl-acts {
		display: flex;
		gap: 6px;
	}
	.sl-small {
		font-size: 11.5px;
		margin-top: 2px;
	}
	.sl-log {
		margin-top: 6px;
	}
	.sl-ua {
		max-width: 220px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	@media (max-width: 560px) {
		.sl-two {
			grid-template-columns: 1fr;
		}
	}
</style>
