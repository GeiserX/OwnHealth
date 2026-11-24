<script lang="ts">
	// Family > Emergency access you can ask for (GET /break-glass): the profiles that named the person
	// as an emergency contact, what each would let them read, and their last request with where it
	// stands. Asking needs a reason and a step-up. Nothing shows for a person nobody named.
	import { askBreakGlass, myBreakGlass } from '#lib/api/breakglass';
	import { getMe } from '#lib/api/core';
	import type { BreakGlassNamed } from '#lib/api/types';
	import { Loader, errRaw, errText, scopeText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { intlLocale } from '#lib/i18n/langs';
	import { phase, statusLine, when } from '#lib/logic/breakglass';
	import { goto } from '$app/navigation';
	import { tick } from 'svelte';
	import { app, setProfile } from '#lib/state/app.svelte';

	const named = new Loader<{ items: BreakGlassNamed[] }>();
	const load = () => named.run(myBreakGlass);
	$effect(() => {
		load();
	});
	let now = $state(new Date());
	$effect(() => {
		const id = setInterval(() => (now = new Date()), 30_000);
		return () => clearInterval(id);
	});
	const loc = $derived(intlLocale(i18n.lang));
	const items = $derived(named.data?.items ?? []);

	// The profile whose request form is open, its reason, and the error it got.
	let asking = $state('');
	let reason = $state('');
	let busy = $state(false);
	let error = $state<{ text: string; raw: string } | null>(null);

	const open = (n: BreakGlassNamed) => !!n.request && ['requested', 'active'].includes(n.request.status) && phase(n.request, now) !== 'expired';
	function start(slug: string) {
		asking = slug;
		reason = '';
		error = null;
	}
	function send(e: SubmitEvent, n: BreakGlassNamed) {
		e.preventDefault();
		busy = true;
		error = null;
		withStepUp(() => askBreakGlass(n.profile.id, reason.trim()))
			.then(async () => {
				asking = '';
				await load();
				// the form is gone: the keyboard lands on the line that says where the request stands
				await tick();
				document.getElementById(`bg-st-${n.profile.slug}`)?.focus();
			})
			.catch((e) => (error = { text: errText(e), raw: errRaw(e) }))
			.finally(() => (busy = false));
	}
	// An open request adds the profile to the person's list (`/me`, break_glass: true).
	async function openRecords(slug: string) {
		app.me = await getMe();
		setProfile(slug);
		await goto('/');
	}
	const statusText = (n: BreakGlassNamed) => {
		if (!n.request) return '';
		const s = statusLine(n.request, 'contact', now);
		return tf(s.key, { at: when(s.at, loc) });
	};
</script>

{#if items.length}
	<section class="card bg" aria-labelledby="bg-mine-h" data-testid="bg-contact-card">
		<h3 id="bg-mine-h">{t('bgl_mine')}<span class="r">{t('h1_bg_r')}</span></h3>
		<div class="note">{t('bgl_mine_d')}</div>
		<ul class="rows">
			{#each items as n (n.profile.id)}
				<li>
					<div class="who"><b>{n.profile.display_name}</b> <span class="muted">{tp('bgl_opens_after', n.delay_hours)}</span></div>
					<div class="muted">{tf('bgl_reads', { list: n.categories.map(scopeText).join('; ') })}</div>
					{#if n.request}
						<div class="status" role="status" id="bg-st-{n.profile.slug}" tabindex="-1">{statusText(n)}</div>
					{/if}
					{#if n.request && phase(n.request, now) === 'active'}
						<div class="p2row"><button class="btn" onclick={() => openRecords(n.profile.slug)}>{tf('bgl_open_records', { name: n.profile.display_name })}</button></div>
					{:else if asking === n.profile.slug}
						<form class="p2form" onsubmit={(e) => send(e, n)} use:focusOnOpen={{ oncancel: () => (asking = '') }}>
							<label>{t('bgl_ask_reason')}
								<textarea class="p2in" bind:value={reason} required maxlength="500" rows="3" aria-describedby="bg-ask-d-{n.profile.slug}"></textarea>
							</label>
							<div class="note" id="bg-ask-d-{n.profile.slug}">{t('bgl_ask_d')}</div>
							{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
							<div class="p2row">
								<button class="btn pri" type="submit" disabled={busy || !reason.trim()}>{t('bgl_ask_send')}</button>
								<button class="btn" type="button" onclick={() => (asking = '')}>{t('i2_cancel')}</button>
							</div>
						</form>
					{:else if !open(n)}
						<div class="p2row"><button class="btn" onclick={() => start(n.profile.slug)}>{t('bgl_ask')}</button></div>
					{/if}
				</li>
			{/each}
		</ul>
	</section>
{/if}

<style>
	.rows {
		list-style: none;
		margin: 8px 0 0;
		padding: 0;
	}
	.rows li {
		padding: 8px 0;
		border-top: 1px solid var(--line);
		font-size: 12.5px;
	}
	.status {
		margin-top: 4px;
		color: var(--watch);
	}
	.p2row {
		margin-top: 6px;
	}
	textarea {
		display: block;
		width: 100%;
		margin-top: 4px;
		resize: vertical;
	}
</style>
