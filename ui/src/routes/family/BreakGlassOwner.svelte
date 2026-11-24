<script lang="ts">
	// Family > Break-glass, for an owner of the profile on screen (routes/break_glass.rs): the contacts
	// they named and what each would read, the requests (cancel while one waits, end one that is open)
	// with where each stands, and the earlier ones. Naming a contact needs the owner's own calendar feed
	// of the profile and a step-up; cancelling and ending need neither.
	import { cancelBreakGlass, getBreakGlass, putBreakGlassContact, removeBreakGlassContact, revokeBreakGlass } from '#lib/api/breakglass';
	import type { BreakGlassCategory, BreakGlassContact, BreakGlassOwnerView, BreakGlassRequest, Household, ProfileSummary } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, scopeText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { intlLocale } from '#lib/i18n/langs';
	import { canLogIn, contactChoices, endErrorKey, ownerStep, phase, statusLine, when } from '#lib/logic/breakglass';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';

	let { profile, household, me }: { profile: ProfileSummary; household: Household | null; me: string } = $props();

	const view = new Loader<BreakGlassOwnerView>();
	const load = () => view.run(() => getBreakGlass(profile.slug), profile.slug);
	$effect(() => {
		load();
	});
	// The status lines move with the clock while the page stays open.
	let now = $state(new Date());
	$effect(() => {
		const id = setInterval(() => (now = new Date()), 30_000);
		return () => clearInterval(id);
	});
	// A request whose waiting time ran out while the page was open: read the view again once, so it
	// shows open and offers End access.
	let reread = '';
	$effect(() => {
		const due = (view.data?.open_requests ?? []).filter((r) => phase(r, now) === 'opening').map((r) => r.id).join();
		if (due && due !== reread) {
			reread = due;
			load();
		}
	});

	const loc = $derived(intlLocale(i18n.lang));
	const v = $derived(view.data);
	const list = (cats: readonly string[]) => cats.map(scopeText).join('; ');
	const choices = $derived(
		v && household ? contactChoices(household.members, household.grants, profile.slug, me, v.contacts.map((c) => c.user_id)) : []
	);

	// The form names a new contact (editing: '') or changes one (editing: their user id).
	let editing = $state<string | null>(null);
	let who = $state('');
	let cats = $state<BreakGlassCategory[]>([]);
	let delay = $state(48);
	let busy = $state(false);
	let error = $state<{ where: string; text: string; raw: string } | null>(null);
	// The row whose second click waits: 'rm:<user>', 'cancel:<id>', 'revoke:<id>'.
	let confirming = $state('');

	function openForm(c?: BreakGlassContact) {
		if (!v) return;
		editing = c?.user_id ?? '';
		who = c?.user_id ?? '';
		cats = [...(c?.categories ?? v.defaults.categories)];
		delay = c?.delay_hours ?? v.defaults.delay_hours;
		error = null;
	}
	function toggleCat(c: BreakGlassCategory, on: boolean) {
		cats = on ? [...cats, c] : cats.filter((x) => x !== c);
	}
	async function act(where: string, fn: () => Promise<unknown>) {
		busy = true;
		error = null;
		try {
			await withStepUp(fn);
			confirming = '';
			await load();
		} catch (e) {
			const key = endErrorKey(e);
			error = { where, text: key ? t(key) : errText(e), raw: errRaw(e) };
			// a request that moved on while the page was open (it opened, ran out, another owner ended
			// it): show where it stands now and the step that fits it
			if (where.startsWith('req:')) {
				confirming = '';
				await load();
			}
		} finally {
			busy = false;
		}
	}
	function save(e: SubmitEvent) {
		e.preventDefault();
		const user = editing || who;
		if (!user) return;
		act('form', async () => {
			await putBreakGlassContact(profile.slug, user, { categories: cats, delay_hours: delay });
			editing = null;
		});
	}
	const end = (r: BreakGlassRequest, step: 'cancel' | 'revoke') =>
		act('req:' + r.id, () => (step === 'cancel' ? cancelBreakGlass(profile.slug, r.id) : revokeBreakGlass(profile.slug, r.id)));
	const statusText = (r: BreakGlassRequest) => {
		const s = statusLine(r, 'owner', now);
		return tf(s.key, { at: when(s.at, loc) });
	};
</script>

<section class="card bg" aria-labelledby="bg-own-h" data-testid="bg-owner">
	<h3 id="bg-own-h">{t('h1_bg')}<span class="r">{t('h1_bg_r')}</span></h3>
	{#if view.error && !v}
		<NotReady error={view.error} onretry={load} />
	{:else if !v}
		<div class="empty">{t('loading')}</div>
	{:else}
		<div class="note">{tf('bgl_own_d', { name: profile.display_name })}</div>

		<h4>{t('bgl_requests')}</h4>
		{#if !v.open_requests.length}
			<div class="muted">{t('bgl_requests_none')}</div>
		{:else}
			<ul class="rows">
				{#each v.open_requests as r (r.id)}
					{@const step = ownerStep(r, now)}
					<li data-testid="bg-request">
						<div class="who"><b>{tf('bgl_asked', { name: r.contact.display_name, at: when(r.requested_at, loc) })}</b></div>
						<div class="reason"><span class="dim">{t('bgl_reason')}:</span> <q>{r.reason}</q></div>
						<div class="status" class:warn={step === 'revoke'} role="status">{statusText(r)}</div>
						<div class="p2row">
							{#if step && confirming === `${step}:${r.id}`}
								<ConfirmRow
									text={tf(step === 'cancel' ? 'bgl_cancel_q' : 'bgl_revoke_q', { name: r.contact.display_name })}
									confirmLabel={t(step === 'cancel' ? 'bgl_cancel' : 'bgl_revoke')}
									cancelLabel={t('bgl_back')}
									{busy}
									onconfirm={() => end(r, step)}
									oncancel={() => (confirming = '')}
								/>
							{:else if step}
								<button class="btn" disabled={busy} onclick={() => (confirming = `${step}:${r.id}`)}>{t(step === 'cancel' ? 'bgl_cancel' : 'bgl_revoke')}</button>
							{/if}
							<a class="btn" href="/family/break-glass/{r.id}">{t('bgl_details')}</a>
						</div>
						{#if error?.where === 'req:' + r.id}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
					</li>
				{/each}
			</ul>
		{/if}
		{#if error && error.where.startsWith('req:') && !v.open_requests.some((r) => 'req:' + r.id === error?.where)}
			<div class="p2err" role="alert" title={error.raw}>{error.text}</div>
		{/if}

		<h4>{t('bgl_contacts')}</h4>
		{#if !v.calendar_feed}
			<div class="note warn" data-testid="bg-need-feed">{t('bgl_need_feed')} <a href="/settings">{t('bgl_need_feed_go')}</a></div>
		{/if}
		{#if !v.contacts.length}
			<div class="muted">{t('bgl_no_contacts')}</div>
		{:else}
			<ul class="rows">
				{#each v.contacts as c (c.user_id)}
					<li data-testid="bg-contact">
						<div class="who"><b>{c.display_name}</b> <span class="muted">{tp('bgl_after', c.delay_hours)}</span></div>
						<div class="muted">{tf('bgl_reads', { list: list(c.categories) })}</div>
						<div class="p2row">
							{#if confirming === 'rm:' + c.user_id}
								<ConfirmRow
									text={tf('bgl_remove_q', { name: c.display_name })}
									confirmLabel={t('bgl_remove')}
									{busy}
									onconfirm={() => act('rm:' + c.user_id, () => removeBreakGlassContact(profile.slug, c.user_id))}
									oncancel={() => (confirming = '')}
								/>
							{:else}
								<button class="btn" disabled={busy || !v.calendar_feed} onclick={() => openForm(c)}>{t('bgl_edit')}</button>
								<button class="btn" disabled={busy} onclick={() => (confirming = 'rm:' + c.user_id)}>{t('bgl_remove')}</button>
							{/if}
						</div>
						{#if error?.where === 'rm:' + c.user_id}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
					</li>
				{/each}
			</ul>
		{/if}

		{#if editing !== null}
			<form class="p2form" onsubmit={save} aria-labelledby="bg-form-h" use:focusOnOpen={{ oncancel: () => ((editing = null), (error = null)) }}>
				<h4 id="bg-form-h">{editing ? (v.contacts.find((c) => c.user_id === editing)?.display_name ?? '') : t('bgl_add')}</h4>
				{#if !editing}
					<label>{t('bgl_who')}
						<select class="p2in" bind:value={who} required>
							<option value="" disabled>{t('bgl_pick')}</option>
							{#each choices as m (m.user_id)}
								<!-- no login yet (an invite not redeemed): they could not ask -->
								<option value={m.user_id} disabled={!canLogIn(m)}>{canLogIn(m) ? m.display_name : tf('bgl_no_login', { name: m.display_name })}</option>
							{/each}
						</select>
					</label>
				{/if}
				<fieldset>
					<legend>{t('bgl_cats')}</legend>
					{#each v.defaults.allowed as c (c)}
						<label class="cat"><input type="checkbox" checked={cats.includes(c)} onchange={(e) => toggleCat(c, e.currentTarget.checked)} /> {scopeText(c)}</label>
					{/each}
				</fieldset>
				<label>{t('bgl_delay')}
					<input class="p2in" type="number" min={v.defaults.min_delay_hours} max={v.defaults.max_delay_hours} step="1" bind:value={delay} required style="width:90px" />
				</label>
				<div class="note">{tf('bgl_delay_d', { min: v.defaults.min_delay_hours, max: v.defaults.max_delay_hours })}</div>
				{#if error?.where === 'form'}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
				<div class="p2row">
					<button class="btn pri" type="submit" disabled={busy || !cats.length || !(editing || who)}>{t('bgl_save')}</button>
					<button class="btn" type="button" onclick={() => ((editing = null), (error = null))}>{t('i2_cancel')}</button>
				</div>
			</form>
		{:else if v.calendar_feed}
			{#if choices.length}
				<div class="p2row"><button class="btn" onclick={() => openForm()}>{t('bgl_add')}</button></div>
			{:else if household}
				<div class="muted">{t('bgl_none_left')}</div>
			{/if}
		{/if}

		{#if v.requests.length}
			<details>
				<summary>{t('bgl_past')} ({v.requests.length})</summary>
				<ul class="rows">
					{#each v.requests as r (r.id)}
						<li>
							<div class="who">{tf('bgl_asked', { name: r.contact.display_name, at: when(r.requested_at, loc) })}</div>
							<div class="reason"><span class="dim">{t('bgl_reason')}:</span> <q>{r.reason}</q></div>
							<div class="muted">{statusText(r)}</div>
						</li>
					{/each}
				</ul>
			</details>
		{/if}
	{/if}
</section>

<style>
	h4 {
		margin: 12px 0 6px;
		font-size: 12px;
		font-weight: 620;
		color: var(--muted);
		text-transform: uppercase;
		letter-spacing: 0.4px;
	}
	.rows {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.rows li {
		padding: 8px 0;
		border-top: 1px solid var(--line);
		font-size: 12.5px;
	}
	.rows li:first-child {
		border-top: 0;
	}
	.reason q {
		overflow-wrap: anywhere;
	}
	.status {
		margin-top: 4px;
		color: var(--watch);
	}
	.status.warn {
		color: var(--out);
	}
	.note.warn {
		color: var(--watch);
	}
	.p2row {
		margin-top: 6px;
	}
	fieldset {
		border: 1px solid var(--line);
		border-radius: 8px;
		margin: 8px 0;
		padding: 6px 10px;
	}
	legend {
		font-size: 12px;
		color: var(--muted);
	}
	.cat {
		display: block;
		font-size: 12.5px;
		margin: 3px 0;
	}
	details {
		margin-top: 12px;
	}
	summary {
		cursor: pointer;
		font-size: 12.5px;
		color: var(--muted);
	}
</style>
