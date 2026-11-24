<script lang="ts">
	// The cancel screen of one break-glass request: where the cancel link of the owner's calendar event
	// lands (routes/ics.rs). It reads the request by its id (GET /break-glass/requests/{id}, which answers
	// only an owner of its profile), shows who asked, why, what they would read and where it stands, and
	// cancels it while it waits or ends it while it is open. The link needs a login, as every page does,
	// and no step-up: taking access away only narrows it, and the server asks none for it either.
	import { page } from '$app/state';
	import { cancelBreakGlass, getBreakGlassRequest, revokeBreakGlass } from '#lib/api/breakglass';
	import { ApiError } from '#lib/api/core';
	import type { BreakGlassRequest } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, scopeText, tf } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { intlLocale } from '#lib/i18n/langs';
	import { endErrorKey, ownerStep, phase, statusLine, when } from '#lib/logic/breakglass';
	import { app } from '#lib/state/app.svelte';
	import '#lib/components/pages2/pages2.css';

	const id = $derived(page.params.id ?? '');
	const found = new Loader<{ slug: string; name: string; request: BreakGlassRequest } | null>();

	async function look(): Promise<{ slug: string; name: string; request: BreakGlassRequest } | null> {
		try {
			const one = await getBreakGlassRequest(id);
			return { slug: one.profile.slug, name: one.profile.display_name, request: one.request };
		} catch (e) {
			// not on a profile this person owns, gone, or not a request id at all
			if (e instanceof ApiError && (e.status === 404 || e.status === 422)) return null;
			throw e;
		}
	}
	// set once the first look ended, so "not found" never flashes before it
	let tried = $state(false);
	const load = () => found.run(look, id).then(() => (tried = true));
	$effect(() => {
		if (app.me) load();
	});

	let now = $state(new Date());
	$effect(() => {
		const tick = setInterval(() => (now = new Date()), 30_000);
		return () => clearInterval(tick);
	});
	const loc = $derived(intlLocale(i18n.lang));
	const hit = $derived(found.data);
	const r = $derived(hit?.request ?? null);
	const step = $derived(r ? ownerStep(r, now) : null);
	// Its waiting time ran out while the page was open: read it again once, so it shows open.
	let reread = '';
	$effect(() => {
		if (r && phase(r, now) === 'opening' && reread !== r.id) {
			reread = r.id;
			load();
		}
	});
	let confirming = $state(false);
	let busy = $state(false);
	let done = $state<'cancel' | 'revoke' | null>(null);
	let error = $state<{ text: string; raw: string } | null>(null);

	async function end() {
		if (!hit || !step) return;
		const s = step;
		busy = true;
		error = null;
		try {
			await (s === 'cancel' ? cancelBreakGlass(hit.slug, hit.request.id) : revokeBreakGlass(hit.slug, hit.request.id));
			done = s;
			confirming = false;
			await load();
		} catch (e) {
			const key = endErrorKey(e);
			error = { text: key ? t(key) : errText(e), raw: errRaw(e) };
			// it moved on while the page was open (it opened, ran out, someone else ended it): show where
			// it stands now, and the step that fits it
			confirming = false;
			await load();
		} finally {
			busy = false;
		}
	}
	const status = $derived.by(() => {
		if (!r) return '';
		const s = statusLine(r, 'owner', now);
		return tf(s.key, { at: when(s.at, loc) });
	});
</script>

<div class="ph">
	<div>
		<h1>{t('bgl_c_t')}</h1>
		{#if r && hit}<div class="sub">{tf('bgl_c_sub', { contact: r.contact.display_name, profile: hit.name })}</div>{/if}
	</div>
	<div class="right"><a class="btn" href="/family">{t('bgl_c_back')}</a></div>
</div>

<section class="card bgc" aria-labelledby="bgc-h">
	<h3 id="bgc-h">{t('h1_bg')}<span class="r">{t('h1_bg_r')}</span></h3>
	{#if found.error}
		<NotReady error={found.error} onretry={load} />
	{:else if !tried || !app.me}
		<div class="empty">{t('loading')}</div>
	{:else if !r}
		<div class="empty" data-testid="bgc-gone">{t('bgl_c_gone')}</div>
	{:else}
		<dl>
			<dt>{t('bgl_c_asked')}</dt>
			<dd class="num">{when(r.requested_at, loc)}</dd>
			<dt>{t('bgl_reason')}</dt>
			<dd><q>{r.reason}</q></dd>
			<dt>{t('bgl_c_reads')}</dt>
			<dd>{r.categories.map(scopeText).join('; ')}</dd>
			<dt>{t('bgl_c_status')}</dt>
			<dd role="status" data-testid="bgc-status">{status}</dd>
		</dl>
		{#if done}<div class="note done" role="status">{t(done === 'cancel' ? 'bgl_c_cancelled' : 'bgl_c_revoked')}</div>{/if}
		{#if step}
			<div class="p2row" data-testid="bgc-step">
				{#if confirming}
					<ConfirmRow
						text={tf(step === 'cancel' ? 'bgl_cancel_q' : 'bgl_revoke_q', { name: r.contact.display_name })}
						confirmLabel={t(step === 'cancel' ? 'bgl_cancel' : 'bgl_revoke')}
						cancelLabel={t('bgl_back')}
						{busy}
						onconfirm={end}
						oncancel={() => (confirming = false)}
					/>
				{:else}
					<button class="btn pri" onclick={() => (confirming = true)}>{t(step === 'cancel' ? 'bgl_cancel' : 'bgl_revoke')}</button>
				{/if}
			</div>
		{/if}
		{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
	{/if}
</section>

<style>
	.bgc {
		max-width: 640px;
	}
	dl {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: 6px 14px;
		margin: 4px 0 10px;
		font-size: 13px;
	}
	dt {
		color: var(--muted);
	}
	dd {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.done {
		color: var(--ok);
	}
	@media (max-width: 760px) {
		dl {
			grid-template-columns: 1fr;
		}
		dd {
			margin-bottom: 6px;
		}
	}
</style>
