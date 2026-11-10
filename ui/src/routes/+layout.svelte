<script lang="ts">
	// The app shell from the mockup: sidebar (brand, household profiles, views), top bar, main view.
	import '#lib/styles/app.css';
	import { onMount, tick, type Snippet } from 'svelte';
	import { afterNavigate, goto } from '$app/navigation';
	import { page } from '$app/state';
	import { ApiError, getMe } from '#lib/api/core';
	import { i18n, setLang, t } from '#lib/i18n/index.svelte';
	import { NAV } from '#lib/nav';
	import { isTypingTarget } from '#lib/logic/keys';
	import { searchTarget } from '#lib/logic/search';
	import LoginScreen from '#lib/components/pages2/LoginScreen.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { AVATAR_COLORS, app, currentProfile, setProfile } from '#lib/state/app.svelte';

	let { children }: { children: Snippet } = $props();
	let search = $state('');
	let searchEl: HTMLInputElement | undefined = $state();
	let sideEl: HTMLElement | undefined = $state();
	let mainEl: HTMLElement | undefined = $state();
	let sideFade = $state(false);
	let loadError = $state<unknown>(null);
	const today = new Date().toISOString().slice(0, 10);

	async function load() {
		app.loading = true;
		try {
			app.me = await getMe();
			app.unauthenticated = false;
			app.error = null;
			if (!app.me.profiles.some((p) => p.slug === app.profile)) {
				const own = app.me.profiles.find((p) => p.is_self) ?? app.me.profiles[0];
				if (own) setProfile(own.slug);
			}
		} catch (e) {
			app.unauthenticated = e instanceof ApiError && e.status === 401;
			app.error = e instanceof Error ? e.message : String(e);
			loadError = e;
		} finally {
			app.loading = false;
		}
	}

	onMount(() => {
		document.documentElement.lang = i18n.lang;
		load();
		const onKey = (e: KeyboardEvent) => {
			if (e.key === '/' && !isTypingTarget(document.activeElement as HTMLElement | null)) {
				e.preventDefault();
				searchEl?.focus();
			}
		};
		document.addEventListener('keydown', onKey);
		window.addEventListener('resize', measureSide);
		return () => {
			document.removeEventListener('keydown', onKey);
			window.removeEventListener('resize', measureSide);
		};
	});

	// On a phone the sidebar is one scrolling row: fade its right edge while more items wait there.
	function measureSide() {
		const el = sideEl;
		sideFade = !!el && el.scrollLeft + el.clientWidth < el.scrollWidth - 1;
	}
	// After a click in the views, the open view is visible in that row, and focus moves to the page, so a
	// screen reader announces it and Tab starts from its content.
	function showActiveView() {
		document.querySelector('nav.menu a.on')?.scrollIntoView({ inline: 'center', block: 'nearest' });
		measureSide();
	}
	// The profiles arrive after the first paint and push the views to the right: centre again then.
	$effect(() => {
		if (app.me?.profiles.length) tick().then(showActiveView);
	});
	afterNavigate(async (nav) => {
		await tick();
		showActiveView();
		if (nav.type !== 'enter' && nav.from?.url.pathname !== nav.to?.url.pathname) mainEl?.focus({ preventScroll: true });
	});

	// The pages with no app around them: signing in, an invite link, the OAuth consent.
	const BARE = ['/login', '/enroll', '/oauth/consent'];
	const bare = $derived(app.unauthenticated || BARE.includes(page.url.pathname));
	const TITLES: Record<string, string> = {
		'/settings': 'i2_settings',
		'/documents': 'i2_docs',
		'/share': 'fu_share',
		'/login': 'login',
		'/enroll': 'pt_enroll',
		'/oauth/consent': 'pt_consent'
	};
	const pageTitle = $derived.by(() => {
		if (page.error) return t('nf_t');
		if (app.unauthenticated) return t('login');
		const path = page.url.pathname;
		if (TITLES[path]) return t(TITLES[path]);
		const n = NAV.find((x) => active(x.path));
		return n ? t(n.key) : '';
	});

	function onSearch(e: KeyboardEvent) {
		if (e.key !== 'Enter') return;
		// Until full search lands (v0.2), route by keyword and carry the term to the page that filters by it.
		const to = searchTarget(search);
		if (to) goto(to);
	}

	function active(path: string): boolean {
		return path === '/' ? page.url.pathname === '/' : page.url.pathname.startsWith(path);
	}
	const roleLine = (p: { role: string; age: number | null }) => `${t('r_' + p.role)}${p.age != null ? ' · ' + p.age : ''}`;
	const me = $derived(app.me);
	const profile = $derived(currentProfile());
</script>

<svelte:head><title>{pageTitle ? `${pageTitle} · OwnHealth` : 'OwnHealth'}</title></svelte:head>

{#if bare}
	<main class="bare" id="main" tabindex="-1" bind:this={mainEl}>
		{#if app.unauthenticated && page.url.pathname !== '/enroll'}
			<LoginScreen />
		{:else if page.url.pathname === '/oauth/consent' && app.loading && !me}
			<div class="empty">{t('loading')}</div>
		{:else}
			<!-- An invite link works before its person has a login. -->
			{@render children()}
		{/if}
	</main>
{:else}
<a class="skip" href="#main">{t('skip')}</a>
<div class="app">
	<aside class="side" class:fade={sideFade} bind:this={sideEl} onscroll={measureSide}>
		<div class="brand">
			<div class="logo">
				<svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="#6ea8fe" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 12h4l2-5 4 10 2-5h6" /></svg>
			</div>
			<div>OwnHealth<small>{t('selfhosted')} · v{__APP_VERSION__}</small></div>
		</div>
		<div class="plabel">{t('household')}</div>
		<div class="profiles">
			{#each me?.profiles ?? [] as p, i (p.id)}
				<button class="prof" class:on={p.slug === profile?.slug} onclick={() => setProfile(p.slug)} aria-pressed={p.slug === profile?.slug} aria-label={p.display_name} title={p.display_name}>
					<span class="av" style="background:{AVATAR_COLORS[i % AVATAR_COLORS.length]}">{p.initials}</span>
					<span><div class="nm">{p.display_name}</div><div class="rl">{roleLine(p)}</div></span>
					{#if p.flags}<span class="bdg">{p.flags}</span>{/if}
				</button>
			{/each}
		</div>
		<div class="plabel">{t('views')}</div>
		<nav class="menu">
			{#each NAV as n (n.path)}
				<a href={n.path} class:on={active(n.path)} aria-current={active(n.path) ? 'page' : undefined}>
					<svg viewBox="0 0 24 24" aria-hidden="true">{@html n.icon}</svg>{t(n.key)}
					{#if n.countFromReview && me?.review_open}<span class="cnt num" title={t('toreview_hh')}>{me.review_open}</span>{/if}
				</a>
			{/each}
		</nav>
		<div class="foot">
			{#if me?.demo}<span>{t('syn')}</span><br />{/if}<span>{t('nodx')}</span> <span>{t('purpose')}</span><br /><span class="mono" style="font-size:10.5px">{t('agpl')}</span>
		</div>
	</aside>
	<div class="main">
		<header class="top">
			<div class="search">
				<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="#8a93a4" stroke-width="2" aria-hidden="true"><circle cx="11" cy="11" r="7" /><path d="m20 20-3.5-3.5" /></svg>
				<input bind:this={searchEl} bind:value={search} onkeydown={onSearch} placeholder={t('search')} aria-label={t('search')} />
				<span class="kbd">/</span>
			</div>
			<div class="asof">
				<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="#9aa3b2" stroke-width="2" aria-hidden="true"><rect x="3" y="5" width="18" height="16" rx="2" /><path d="M3 10h18M8 3v4M16 3v4" /></svg>
				<span>{t('asof')}</span> <b class="num">{today}</b>
			</div>
			<div class="sp"></div>
			<a class="iconbtn" href="/import-review" title={t('toreview_hh')}>
				<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#e0a84a" stroke-width="2" aria-hidden="true"><path d="M4 4h16v12H4zM4 16l4 4h8l4-4" /></svg>
				<span>{t('toreview')}</span>
				{#if me}<b class="num" style="color:var(--watch)">{me.review_open}</b>{:else}<b class="dim">—</b>{/if}
			</a>
			<div class="seg" role="group" aria-label={t('lang_label')}>
				<button class:on={i18n.lang === 'es'} aria-pressed={i18n.lang === 'es'} onclick={() => setLang('es')}>ES</button>
				<button class:on={i18n.lang === 'en'} aria-pressed={i18n.lang === 'en'} onclick={() => setLang('en')}>EN</button>
			</div>
			{#if me}<a class="av" href="/settings" style="background:#6ea8fe;text-decoration:none" title={me.user.display_name} aria-label={`${t('i2_settings')} · ${me.user.display_name}`}>{me.user.display_name.split(' ').map((w) => w[0]).slice(0, 2).join('')}</a>{/if}
		</header>
		<main class="view" id="main" tabindex="-1" bind:this={mainEl}>
			{#if app.loading && !me}
				<div class="empty">{t('loading')}</div>
			{:else if app.error && !me}
				<div class="card" style="max-width:520px;margin:8vh auto 0"><NotReady error={loadError} onretry={load} /></div>
			{:else}
				{@render children()}
			{/if}
		</main>
	</div>
</div>
{/if}
