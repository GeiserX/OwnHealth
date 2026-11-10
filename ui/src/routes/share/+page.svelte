<script lang="ts">
	// The share screen (PWA share target, Android): files shared from another app arrive here through
	// the service worker; the person picks whose they are and sends them to /api/ingest/share. It also
	// works as a plain file picker, so a phone without the share sheet can use it.
	import '#lib/components/pages2/pages2.css';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { shareFiles } from '#lib/api/inbox';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import { goto } from '$app/navigation';
	import { errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { shareAccepts, shareTone } from '#lib/logic/inbox';
	import { app, currentProfile, setProfile } from '#lib/state/app.svelte';

	type Result = { filename: string; outcome?: string; duplicate?: boolean };
	let files = $state<File[]>([]);
	let profile = $state(currentProfile()?.slug ?? '');
	let waiting = $state(false);
	let busy = $state(false);
	let error = $state<{ text: string; raw: string } | null>(null);
	// Everything sent in this visit, newest batch last: a second send adds to the list.
	let results = $state<Result[] | null>(null);
	// The share sheet opened this page but no file arrived in time.
	let lost = $state(false);
	const profiles = $derived(app.me?.profiles ?? []);
	$effect(() => {
		if (!profile && profiles.length) profile = currentProfile()?.slug ?? profiles[0].slug;
	});

	onMount(() => {
		if (!page.url.searchParams.has('received') || !('serviceWorker' in navigator)) return;
		waiting = true;
		const onMsg = (e: MessageEvent) => {
			if (e.data?.type !== 'share-files') return;
			files = [...files, ...(e.data.files as File[])];
			waiting = false;
		};
		navigator.serviceWorker.addEventListener('message', onMsg);
		navigator.serviceWorker.ready.then(() => navigator.serviceWorker.controller?.postMessage('share-ready'));
		const giveUp = setTimeout(() => {
			if (waiting && !files.length) lost = true;
			waiting = false;
		}, 8000);
		return () => {
			clearTimeout(giveUp);
			navigator.serviceWorker.removeEventListener('message', onMsg);
		};
	});

	function pick(e: Event) {
		const input = e.currentTarget as HTMLInputElement;
		files = [...files, ...Array.from(input.files ?? [])];
		input.value = '';
		lost = false;
	}
	// Only PDFs and images travel; anything else stays listed as not supported and is not sent.
	const sendable = $derived(files.filter(shareAccepts));
	async function send() {
		busy = true;
		error = null;
		try {
			const items = (await shareFiles(profile, sendable)).items;
			const skipped = files.filter((f) => !shareAccepts(f)).map((f) => ({ filename: f.name, outcome: 'unsupported' }));
			results = [...(results ?? []), ...items, ...skipped];
			files = [];
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}
	// The review screen shows the current profile: switch to the one the files went to first.
	function review(e: MouseEvent) {
		e.preventDefault();
		if (profile) setProfile(profile);
		goto('/import-review');
	}
	const size = (n: number) => (n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);
	const whose = $derived(profiles.find((p) => p.slug === profile)?.display_name ?? '');
</script>

<PageHeader title={t('fu_share')} sub={t('fu_share_sub')} />

<div class="wrap">
	<section class="card p2form" aria-labelledby="sh-h">
		<h3 id="sh-h">{waiting ? t('fu_share_waiting') : files.length ? tp('fu_share_n', files.length) : t('fu_share_none')}</h3>
		{#if files.length}
			<ul class="files">
				{#each files as f, i (i)}
					<li>
						<span class="mono nm" class:dim={!shareAccepts(f)}>{f.name}</span>
						{#if !shareAccepts(f)}<span class="pill p-watch"><i aria-hidden="true"></i>{t('fu_share_unsupported')}</span>{/if}
						<span class="dim">{size(f.size)}</span>
						<button class="btn sm" aria-label="{t('i2_remove')} {f.name}" onclick={() => (files = files.filter((_, j) => j !== i))}>×</button>
					</li>
				{/each}
			</ul>
		{/if}
		<label class="btn pick"><input type="file" multiple accept="application/pdf,image/*" onchange={pick} />{t('fu_share_pick')}</label>
		<label
			>{t('fu_share_whose')}
			<select class="p2in" bind:value={profile} required>
				{#each profiles as p (p.id)}<option value={p.slug}>{p.display_name}</option>{/each}
			</select>
		</label>
		<div class="note">{t('fu_share_note')}</div>
		{#if lost}<div class="note" role="status" style="color:var(--watch)">{t('fu_share_lost')}</div>{/if}
		{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
		<button class="btn pri big" disabled={busy || !sendable.length || !profile} aria-busy={busy} onclick={send}>{busy ? t('fu_share_sending') : whose ? tf('fu_share_send', { who: whose }) : t('fu_share_send0')}</button>
	</section>

	{#if results}
		<section class="card" aria-live="polite">
			<h3>{t('fu_share_done')}</h3>
			<ul class="files">
				{#each results as r, i (i)}
					{@const pill = shareTone(r)}
					<li><span class="mono nm">{r.filename}</span><span class="pill {pill.tone}"><i aria-hidden="true"></i>{t(pill.key)}</span></li>
				{/each}
			</ul>
			<a class="btn" href="/import-review" onclick={review}>{t('nav_imp')}</a>
		</section>
	{/if}
</div>

<style>
	.wrap {
		max-width: 560px;
		display: grid;
		gap: 12px;
	}
	.files {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.files li {
		display: flex;
		gap: 8px;
		align-items: center;
		font-size: 12.5px;
	}
	.nm {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.pick {
		justify-content: center;
		cursor: pointer;
	}
	.pick input {
		position: absolute;
		width: 1px;
		height: 1px;
		opacity: 0;
	}
	.pick:focus-within {
		outline: 2px solid var(--accent);
	}
	.big {
		justify-content: center;
		padding: 9px 12px;
		font-size: 13px;
	}
</style>
