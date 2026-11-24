<script lang="ts">
	// The in-app notices (GET /notifications): emergency access to the profiles you own. One entry per
	// request, so twenty reads through it are one line with their count. The badge counts the requests
	// with something unread and stays until Mark all as read. Unread notices open the panel by
	// themselves once per login: the mark of it lives in this browser until the app finds no session
	// (a logout, an expired login), so another tab of the same login leaves it closed and the next
	// login opens it again. Only owners get notices, so nobody else reads them.
	import { onMount, tick } from 'svelte';
	import { getNotices, readNotices } from '#lib/api/breakglass';
	import type { Notices } from '#lib/api/types';
	import { errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { intlLocale } from '#lib/i18n/langs';
	import { NOTICES_SHOWN, groupNotices, unreadGroups, when } from '#lib/logic/breakglass';
	import { app } from '#lib/state/app.svelte';

	/** A tab that comes back reads the notices again, at most once a minute (each read writes an audit row). */
	const REFRESH_MS = 60_000;

	let data = $state<Notices | null>(null);
	let open = $state(false);
	let busy = $state(false);
	let error = $state('');
	let lastLoad = 0;
	let btn: HTMLButtonElement | undefined = $state();
	let panel: HTMLElement | undefined = $state();
	let root: HTMLElement | undefined = $state();
	let closeBtn: HTMLButtonElement | undefined = $state();
	const owner = $derived((app.me?.profiles ?? []).some((p) => p.role === 'owner' && !p.break_glass));
	const user = $derived(owner ? (app.me?.user.id ?? '') : '');
	const groups = $derived(groupNotices(data?.items ?? [], data?.uses));
	const badge = $derived(unreadGroups(groups));
	const loc = $derived(intlLocale(i18n.lang));

	async function load(): Promise<void> {
		lastLoad = Date.now();
		try {
			data = await getNotices();
			error = '';
		} catch (e) {
			error = errText(e);
		}
	}

	// Who is signed in decides whose notices show; a new login reads them and, with news, opens the panel.
	$effect(() => {
		const u = user;
		data = null;
		open = false;
		if (!u) return;
		load().then(() => {
			if (u !== user || !unreadGroups(groupNotices(data?.items ?? [], data?.uses))) return;
			if (localStorage.getItem(NOTICES_SHOWN) === u) return;
			localStorage.setItem(NOTICES_SHOWN, u);
			open = true;
		});
	});

	onMount(() => {
		const onVisible = () => {
			if (document.visibilityState === 'visible' && user && Date.now() - lastLoad > REFRESH_MS) load();
		};
		// a click anywhere else closes the panel, as any menu does: it never sits over the page
		const onDown = (e: PointerEvent) => {
			if (open && root && e.target instanceof Node && !root.contains(e.target)) open = false;
		};
		document.addEventListener('visibilitychange', onVisible);
		document.addEventListener('pointerdown', onDown);
		return () => {
			document.removeEventListener('visibilitychange', onVisible);
			document.removeEventListener('pointerdown', onDown);
		};
	});

	async function toggle() {
		open = !open;
		if (!open) return;
		load();
		await tick();
		panel?.focus();
	}
	function close() {
		open = false;
		btn?.focus();
	}
	async function readAll() {
		busy = true;
		try {
			await readNotices();
			await load();
		} catch (e) {
			error = errText(e);
		} finally {
			busy = false;
		}
		// the button is gone with the news: keep the keyboard in the panel
		await tick();
		closeBtn?.focus();
	}
	function onKey(e: KeyboardEvent) {
		if (e.key !== 'Escape') return;
		e.preventDefault();
		close();
	}
	const step = (kind: string, by: string | null) => tf('nt_k_' + kind.replace('break_glass.', ''), { by: by ?? t('nt_someone') });
</script>

{#if data && data.items.length}
	<div class="nt" bind:this={root}>
		<button
			class="iconbtn"
			bind:this={btn}
			onclick={toggle}
			aria-expanded={open}
			aria-controls="nt-panel"
			aria-label={badge ? tp('nt_btn_new', badge) : t('nt_btn')}
			title={t('nt_btn')}
		>
			<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.94 1.94 0 0 0 3.4 0" /></svg>
			<span>{t('nt_btn')}</span>
			{#if badge}<b class="num badge" data-testid="nt-badge">{badge}</b>{/if}
		</button>
		{#if open}
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
									<b>{tf('nt_group', { contact: g.contact ?? t('nt_someone'), profile: g.profile.display_name })}</b>
									{#if g.unread}<span class="new">{t('nt_new')}</span>{/if}
								</div>
								{#each g.steps as s, i (i)}
									<div class="step" class:muted={!s.unread}>{step(s.kind, s.by)} · <span class="num">{when(s.at, loc)}</span></div>
								{/each}
								{#if g.uses}<div class="step">{tp('nt_uses', g.uses, { at: when(g.lastUse, loc) })}</div>{/if}
								{#if g.request}<a href="/family/break-glass/{g.request}" onclick={() => (open = false)}>{t('nt_see')}</a>{/if}
							</li>
						{/each}
					</ul>
				{/if}
				{#if error}<div class="p2err" role="alert">{error}</div>{/if}
				<div class="acts">
					{#if badge}<button class="btn sm" disabled={busy} onclick={readAll}>{t('nt_read_all')}</button>{/if}
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
