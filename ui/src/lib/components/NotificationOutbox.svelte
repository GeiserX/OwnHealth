<script lang="ts">
	// The in-app notices (GET /notifications): emergency access to the profiles you own. One entry per
	// request, so twenty reads through it are one line with their count. The badge counts the requests
	// with something unread and stays until Mark all as read. Unread notices open the panel by
	// themselves once per login, after the first load that works: the mark of it lives in this browser
	// until the app finds no session (a logout, an expired login), so another tab of the same login
	// leaves it closed and the next login opens it again. Only owners get notices, so nobody else reads
	// them. Only the newest load for the person signed in shows (noticeLoader).
	import { onDestroy, onMount, tick, untrack } from 'svelte';
	import { getNotices, readNotices } from '#lib/api/breakglass';
	import type { Notices } from '#lib/api/types';
	import { errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { intlLocale } from '#lib/i18n/langs';
	import { groupNotices, noticeLoader, unreadGroups, when, type NoticeView } from '#lib/logic/breakglass';
	import { app, currentProfile, setProfile } from '#lib/state/app.svelte';

	/** A tab that comes back reads the notices again, at most once a minute (each read writes an audit row). */
	const REFRESH_MS = 60_000;

	const view = $state<NoticeView>({ data: null, error: null, open: false });
	let busy = $state(false);
	let lastLoad = 0;
	let btn: HTMLButtonElement | undefined = $state();
	let panel: HTMLElement | undefined = $state();
	let root: HTMLElement | undefined = $state();
	let closeBtn: HTMLButtonElement | undefined = $state();
	let readBtn: HTMLButtonElement | undefined = $state();
	// whether the keyboard is on Mark all as read: a focus anywhere else clears it
	let readFocus = false;
	// gone once this panel left the page (the sign-in screen replaced the app)
	let gone = false;
	// The owner signed in, read from the app state on each call: a session that ended mid-use has
	// nobody signed in, so a load still on its way is dropped. A $derived is not used here, because the
	// loader asks after the panel is gone, and a destroyed derived answers its last value.
	function signedIn(): string {
		if (gone || app.unauthenticated || !app.me) return '';
		return app.me.profiles.some((p) => p.role === 'owner' && !p.break_glass) ? app.me.user.id : '';
	}
	const user = $derived(signedIn());
	const data = $derived<Notices | null>(view.data);
	const groups = $derived(groupNotices(data?.items ?? [], data?.uses, data?.ice));
	const badge = $derived(unreadGroups(groups));
	const loc = $derived(intlLocale(i18n.lang));
	const store = { getItem: (k: string) => localStorage.getItem(k), setItem: (k: string, v: string) => localStorage.setItem(k, v) };
	const load = noticeLoader(
		view,
		signedIn,
		() => {
			lastLoad = Date.now();
			return getNotices();
		},
		store
	);

	// Who is signed in decides whose notices show; a new login reads them and, with news, opens the panel.
	$effect(() => {
		const u = user;
		view.data = null;
		view.error = null;
		view.open = false;
		if (u) untrack(load);
	});

	onDestroy(() => (gone = true));

	onMount(() => {
		const onVisible = () => {
			if (document.visibilityState === 'visible' && user && Date.now() - lastLoad > REFRESH_MS) load();
		};
		const onFocus = (e: FocusEvent) => {
			readFocus = !!readBtn && e.target === readBtn;
		};
		// a click anywhere else closes the panel, as any menu does: it never sits over the page
		const onDown = (e: PointerEvent) => {
			if (view.open && root && e.target instanceof Node && !root.contains(e.target)) view.open = false;
		};
		document.addEventListener('visibilitychange', onVisible);
		document.addEventListener('pointerdown', onDown);
		document.addEventListener('focusin', onFocus);
		return () => {
			document.removeEventListener('visibilitychange', onVisible);
			document.removeEventListener('pointerdown', onDown);
			document.removeEventListener('focusin', onFocus);
		};
	});

	async function toggle() {
		view.open = !view.open;
		if (!view.open) return;
		load();
		await tick();
		panel?.focus();
	}
	function close() {
		view.open = false;
		btn?.focus();
	}
	async function readAll() {
		busy = true;
		try {
			await readNotices();
			await load();
		} catch (e) {
			view.error = e;
		} finally {
			busy = false;
		}
		// the button goes with the news: when the keyboard was still on it, keep it in the panel; a
		// person who moved on while it ran keeps their place
		await tick();
		const lost = !document.activeElement || document.activeElement === document.body;
		if (readFocus && lost) (readBtn ?? closeBtn)?.focus();
	}
	function onKey(e: KeyboardEvent) {
		if (e.key !== 'Escape') return;
		e.preventDefault();
		close();
	}
	// Share links shows the profile on screen: a card notice for another profile puts that one on screen first
	function openCards(slug: string) {
		if (currentProfile()?.slug !== slug) setProfile(slug);
		view.open = false;
	}
	const step = (kind: string, by: string | null) => tf('nt_k_' + kind.replace('break_glass.', ''), { by: by ?? t('nt_someone') });
</script>

{#if data && data.items.length}
	<div class="nt" bind:this={root}>
		<button
			class="iconbtn"
			bind:this={btn}
			onclick={toggle}
			aria-expanded={view.open}
			aria-controls="nt-panel"
			aria-label={badge ? tp('nt_btn_new', badge) : t('nt_btn')}
			title={t('nt_btn')}
		>
			<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.94 1.94 0 0 0 3.4 0" /></svg>
			<span>{t('nt_btn')}</span>
			{#if badge}<b class="num badge" data-testid="nt-badge">{badge}</b>{/if}
		</button>
		{#if view.open}
			<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
			<section id="nt-panel" class="card panel" aria-labelledby="nt-h" tabindex="-1" bind:this={panel} onkeydown={onKey}>
				<h3 id="nt-h">{t('nt_t')}<span class="r">{t('nt_d')}</span></h3>
				{#if !groups.length}
					<div class="empty">{t('nt_empty')}</div>
				{:else}
					<ul>
						{#each groups as g (g.key)}
							<li class:unread={g.unread > 0}>
								<div class="who">
									<b>{g.card ? tf('nt_ice_group', { profile: g.profile.display_name }) : tf('nt_group', { contact: g.contact ?? t('nt_someone'), profile: g.profile.display_name })}</b>
									{#if g.unread}<span class="new">{t('nt_new')}</span>{/if}
								</div>
								{#each g.steps as s, i (i)}
									<div class="step" class:muted={!s.unread}>{step(s.kind, s.by)} · <span class="num">{when(s.at, loc)}</span></div>
								{/each}
								{#if g.uses}<div class="step">{tp(g.card ? 'nt_ice_opens' : 'nt_uses', g.uses, { at: when(g.lastUse, loc) })}</div>{/if}
								{#if g.card}<a href="/links" onclick={() => openCards(g.profile.slug)}>{t('ice_manage')}</a>{/if}
								{#if g.request}<a href="/family/break-glass/{g.request}" onclick={() => (view.open = false)}>{t('nt_see')}</a>{/if}
							</li>
						{/each}
					</ul>
				{/if}
				{#if view.error}<div class="p2err" role="alert">{errText(view.error)}</div>{/if}
				<div class="acts">
					{#if badge}<button class="btn sm" bind:this={readBtn} disabled={busy} onclick={readAll}>{t('nt_read_all')}</button>{/if}
					<button class="btn sm" bind:this={closeBtn} onclick={close}>{t('nt_close')}</button>
				</div>
			</section>
		{/if}
	</div>
{/if}

<style>
	.nt {
		position: relative;
	}
	.badge {
		color: var(--out);
	}
	.panel {
		position: absolute;
		top: calc(100% + 6px);
		right: 0;
		z-index: 30;
		width: min(400px, 92vw);
		max-height: 70vh;
		overflow: auto;
		box-shadow: 0 8px 24px rgba(0, 0, 0, 0.45);
	}
	/* Phone: the button is not at the right edge, so the panel spans the screen under it. */
	@media (max-width: 760px) {
		.panel {
			position: fixed;
			top: auto;
			left: 8px;
			right: 8px;
			width: auto;
		}
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	li {
		padding: 8px 0;
		border-top: 1px solid var(--line);
		font-size: 12.5px;
	}
	li.unread .who b {
		color: var(--text);
	}
	.who {
		display: flex;
		gap: 8px;
		align-items: baseline;
		margin-bottom: 2px;
	}
	.new {
		font-size: 10.5px;
		color: var(--watch);
		text-transform: uppercase;
		letter-spacing: 0.4px;
	}
	.step {
		margin-top: 2px;
	}
	li a {
		display: inline-block;
		margin-top: 4px;
		color: var(--accent);
		font-size: 12px;
	}
	.acts {
		display: flex;
		gap: 6px;
		justify-content: flex-end;
		margin-top: 8px;
	}
</style>
