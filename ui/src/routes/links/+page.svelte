<script lang="ts">
	// Share links: the owner or a guardian sends the profile's summary to a doctor as a SMART Health
	// Link (backend routes/shl.rs). New link: what it carries, expiry, an optional PIN, the language.
	// The link and its QR code show once, right after it is made, and live only in this page's memory:
	// the server keeps no copy of the key. Each link says its state, who opened it, and can be revoked.
	// The PIN input has no name, so no form submit can ever put it in a URL; it is cleared once used.
	import '#lib/components/pages2/pages2.css';
	import type { ShlAccess, ShlCreated, ShlLink } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import { ago, day, stamp } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { copyText } from '#lib/logic/inbox';
	import { currentProfile } from '#lib/state/app.svelte';
	import { createLink, linkAccess, listLinks, revokeLink } from './api';
	import { DAYS, DEFAULT_DAYS, PICKS, PIN_MAX, accessKey, categoriesFor, mayShare, pinProblem, revocable, shownCategories, stateOf } from './logic';
	import { qrOf } from './qr';

	const prof = $derived(currentProfile());
	const slug = $derived(prof?.slug ?? '');
	const name = $derived(prof?.display_name ?? '');
	const sharer = $derived(mayShare(prof?.role));
	const held = $derived((prof?.scopes ?? []) as string[]);

	const links = new Loader<ShlLink[]>();
	const log = new Loader<ShlAccess[]>();
	let label = $state('');
	let picked = $state<string[]>([...PICKS]);
	let days = $state(DEFAULT_DAYS);
	let pin = $state('');
	let lang = $state<'es' | 'en'>(i18n.lang);
	let busy = $state(false);
	let error = $state<{ text: string; raw: string } | null>(null);
	// The link just made: in this page's memory only, gone on reload or on another profile.
	let fresh = $state<{ id: string; label: string | null; shlink: string; pin: boolean; leftOut: number | null; qr: { size: number; path: string } } | null>(null);
	let copied = $state<boolean | null>(null);
	let linkEl: HTMLElement | undefined = $state();
	let confirming = $state<string | null>(null);
	let logOf = $state<string | null>(null);
	let rowError = $state<{ id: string; text: string; raw: string } | null>(null);

	const load = () => links.run(async () => (await listLinks(slug)).items, slug);
	$effect(() => {
		const s = slug;
		const ok = sharer;
		fresh = null;
		copied = null;
		confirming = null;
		logOf = null;
		error = null;
		if (s && ok) load();
	});
	const pinWrong = $derived(pinProblem(pin));

	async function make() {
		busy = true;
		error = null;
		try {
			const typed = pin;
			const made: ShlCreated = await withStepUp(() =>
				createLink(slug, {
					categories: categoriesFor(picked, held),
					days,
					passcode: typed || undefined,
					label: label.trim() || undefined,
					lang
				})
			);
			fresh = {
				id: made.link.id,
				label: made.link.label,
				shlink: made.shlink,
				pin: made.link.passcode,
				leftOut: made.left_out?.sensitive ?? null,
				qr: qrOf(made.shlink)
			};
			copied = null;
			pin = '';
			label = '';
			load();
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}
	async function revoke(x: ShlLink) {
		busy = true;
		rowError = null;
		try {
			await revokeLink(slug, x.id);
			confirming = null;
			if (fresh?.id === x.id) fresh = null;
			load();
			if (logOf === x.id) openLog(x.id);
		} catch (e) {
			rowError = { id: x.id, text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}
	function openLog(id: string) {
		logOf = id;
		log.run(async () => (await linkAccess(slug, id)).items, id);
	}
	async function copy() {
		if (fresh) copied = await copyText(typeof navigator === 'undefined' ? null : navigator.clipboard, fresh.shlink, linkEl);
	}
	const title = (x: { label: string | null }) => x.label ?? t('shl_nolabel');
	const cat = (c: string) => t('shl_c_' + c.replace('read:', ''));
	// One line of what a link carries, one of its dates and use.
	function carries(x: ShlLink): string {
		const parts = shownCategories(x.categories).map((c) => t('shl_s_' + c.replace('read:', '')));
		if (x.passcode) parts.push(x.attempts_left !== null && revocable(x) ? `${t('shl_pin_on')} (${tp('shl_tries', x.attempts_left)})` : t('shl_pin_on'));
		return parts.join(' · ');
	}
	function when(x: ShlLink): string {
		const parts = [`${t('i2_added')} ${day(x.created_at)}`, `${x.state === 'expired' ? t('shl_expired_on') : t('shl_expires')} ${day(x.expires_at)}`];
		parts.push(x.opened ? tp('shl_opened', x.opened) : t('shl_opened0'));
		if (x.last_access_at) parts.push(`${t('shl_last')} ${ago(x.last_access_at)}`);
		return parts.join(' · ');
	}
</script>

<PageHeader title={t('shl_nav')} sub={name && sharer ? tf('shl_sub', { name }) : undefined} />

{#if prof && !sharer}
	<div class="card"><div class="empty" role="status">{tf('shl_who_no', { name })}</div></div>
{:else if prof}
	<div class="sl-wrap">
		<section class="card p2form" aria-labelledby="shl-new">
			<h3 id="shl-new">{t('shl_new')}<span class="r">{t('i2_tok_stepup')}</span></h3>
			<label
				>{t('shl_label')}
				<input class="p2in" maxlength="80" autocomplete="off" placeholder={t('shl_label_ph')} bind:value={label} />
			</label>
			<fieldset>
				<legend>{t('shl_what')}</legend>
				<label class="sl-chk"><input type="checkbox" checked disabled />{t('shl_c_summary')}</label>
				{#each PICKS as c (c)}
					<label class="sl-chk" class:dim={!held.includes(c)}>
						<input type="checkbox" value={c} bind:group={picked} disabled={!held.includes(c)} />{cat(c)}
						{#if !held.includes(c)}<span class="dim">· {t('shl_c_closed')}</span>{/if}
					</label>
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
						<option value="es">Español</option>
						<option value="en">English</option>
					</select>
				</label>
			</div>
			<label
				>{t('shl_pin')}
				<input class="p2in" type="password" autocomplete="off" spellcheck="false" maxlength={PIN_MAX} aria-invalid={!!pinWrong} aria-describedby="shl-pin-d" bind:value={pin} />
			</label>
			<div class="note" id="shl-pin-d">{pinWrong ? t(pinWrong) : t('shl_pin_d')}</div>
			{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
			<div class="p2row">
				<button class="btn pri" disabled={busy || !!pinWrong} aria-busy={busy} onclick={make}>{busy ? t('shl_making') : t('shl_make')}</button>
			</div>
		</section>

		{#if fresh}
			<section class="card sl-fresh" aria-labelledby="shl-made" aria-live="polite">
				<h3 id="shl-made">{fresh.label ? tf('shl_made', { label: fresh.label }) : t('shl_made0')}</h3>
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
					{#each links.data as x (x.id)}
						{@const st = stateOf(x)}
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
									<button class="btn sm" aria-expanded={logOf === x.id} aria-label="{t('shl_log')} · {title(x)}" onclick={() => (logOf === x.id ? (logOf = null) : openLog(x.id))}>{t('shl_log')}</button>
									{#if revocable(x)}
										<button class="btn sm no" disabled={busy} aria-expanded={confirming === x.id} aria-label="{t('i2_revoke')} · {title(x)}" onclick={() => (confirming = x.id)}>{t('i2_revoke')}</button>
									{/if}
								</div>
							</div>
							{#if confirming === x.id}
								<ConfirmRow text={t('shl_revoke_d')} confirmLabel={t('i2_revoke')} {busy} onconfirm={() => revoke(x)} oncancel={() => (confirming = null)} />
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
