<script lang="ts">
	// The cancel screen of one break-glass request: where the cancel link of the owner's calendar event
	// lands (routes/ics.rs). It finds the request on the profiles the person owns, shows who asked, why,
	// what they would read and where it stands, and cancels it while it waits or ends it while it is
	// open. A link opened from a calendar app can come from any device, so the screen confirms it is the
	// owner first: a login in the last minutes counts, else the step-up dialog asks.
	import { page } from '$app/state';
	import { cancelBreakGlass, getBreakGlass, revokeBreakGlass } from '#lib/api/breakglass';
	import type { BreakGlassOwnerView, BreakGlassRequest } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, scopeText, tf } from '#lib/components/pages1/util.svelte';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import { confirmFresh } from '#lib/components/pages2/stepup.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { intlLocale } from '#lib/i18n/langs';
	import { findRequest, ownerStep, statusLine, when } from '#lib/logic/breakglass';
	import { app } from '#lib/state/app.svelte';
	import '#lib/components/pages2/pages2.css';

	const id = $derived(page.params.id ?? '');
	const owned = $derived((app.me?.profiles ?? []).filter((p) => p.role === 'owner' && !p.break_glass));
	const found = new Loader<{ slug: string; name: string; request: BreakGlassRequest } | null>();

	async function look(): Promise<{ slug: string; name: string; request: BreakGlassRequest } | null> {
		const views: (BreakGlassOwnerView & { slug: string })[] = [];
		for (const p of owned) views.push({ slug: p.slug, ...(await getBreakGlass(p.slug)) });
		const hit = findRequest(views, id);
		return hit ? { ...hit, name: owned.find((p) => p.slug === hit.slug)?.display_name ?? hit.slug } : null;
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
			// the person backed out of the confirmation: nothing changes
			if (!(await confirmFresh())) return;
			await (s === 'cancel' ? cancelBreakGlass(hit.slug, hit.request.id) : revokeBreakGlass(hit.slug, hit.request.id));
			done = s;
			confirming = false;
			await load();
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
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
			<div class="p2row">
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
			<div class="note">{t('bgl_c_su')}</div>
		{/if}
		{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
	{/if}
</section>

<StepUpDialog />

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
