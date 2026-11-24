<script lang="ts">
	import { onMount } from 'svelte';
	import { ApiError, get, p, post } from '#lib/api/core';
	import { tick } from 'svelte';
	import { day } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { tf } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { LANGS, type Lang } from '#lib/i18n/langs';
	import { iceUntil, utcMidnights } from '#lib/logic/ice';

	let { slug }: { slug: string } = $props();
	type Contact = { name: string; phone: string };
	type Item = { id: string; kind: string; name: string; sensitive: boolean };
	type Options = { online: boolean; items: Item[] };
	type Files = { pdf: string; png: string; expires_at: string };

	let options = $state<Options | null>(null);
	let contacts = $state<Contact[]>([{ name: '', phone: '' }]);
	// the card is for whoever reads it nearby: it starts in the screen's language, not the sheet's
	let lang = $state<Lang>(i18n.lang);
	let selected = $state<string[]>([]);
	let leftOut = $state<string[]>([]);
	let online = $state(false);
	let days = $state(7);
	let pending = $state(false);
	let problem = $state('');
	let files = $state<Files | null>(null);
	let shownOpen = $state(false);
	let ready = $state<HTMLElement | null>(null);
	let mounted = false;

	const plainItems = $derived(options?.items.filter((item) => !item.sensitive) ?? []);
	const sensitiveItems = $derived(options?.items.filter((item) => item.sensitive) ?? []);
	// The server's rule: 00:00 UTC, the chosen number of days after today's UTC date. The clock moves at
	// 00:00 UTC and when the tab comes back, so a page left open overnight previews the right date.
	let now = $state(Date.now());
	const until = $derived(iceUntil(days, now));

	function clearFiles() {
		if (files) {
			URL.revokeObjectURL(files.pdf);
			URL.revokeObjectURL(files.png);
		}
		files = null;
	}

	function errorKey(e: unknown): string {
		const code = e instanceof ApiError ? e.code : '';
		const specific = ['ice_closed', 'ice_contact', 'ice_picks', 'ice_full', 'ice_sharing_off', 'ice_share_url'];
		return specific.includes(code) ? code : 'ice_failed';
	}

	async function readOptions() {
		problem = '';
		try {
			const available = await get<Options>(`${p(slug)}/ice`);
			if (!mounted) return;
			options = available;
			selected = selected.filter((id) => available.items.some((item) => item.id === id && item.sensitive));
			leftOut = leftOut.filter((id) => available.items.some((item) => item.id === id && !item.sensitive));
			if (!available.online) online = false;
		} catch (e) {
			if (mounted) problem = errorKey(e);
		}
	}

	onMount(() => {
		mounted = true;
		void readOptions();
		const stop = utcMidnights(() => (now = Date.now()));
		const onVisible = () => {
			if (document.visibilityState === 'visible') now = Date.now();
		};
		document.addEventListener('visibilitychange', onVisible);
		return () => {
			mounted = false;
			stop();
			document.removeEventListener('visibilitychange', onVisible);
			clearFiles();
		};
	});

	function keep(id: string, on: boolean) {
		leftOut = on ? leftOut.filter((x) => x !== id) : [...leftOut, id];
	}

	function urlFor(base64: string, mime: string): string {
		const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
		return URL.createObjectURL(new Blob([bytes], { type: mime }));
	}

	async function issue() {
		problem = '';
		pending = true;
		const body = {
			lang, days, online, sensitive_ids: [...selected], left_out: [...leftOut],
			contacts: contacts.filter((c) => c.name.trim() || c.phone.trim()).map((c) => ({ name: c.name.trim(), phone: c.phone.trim() }))
		};
		try {
			const made = await withStepUp(() => post<Files>(`${p(slug)}/ice`, body));
			if (!mounted) return;
			clearFiles();
			files = {
				pdf: urlFor(made.pdf, 'application/pdf'),
				png: urlFor(made.png, 'image/png'),
				expires_at: made.expires_at
			};
			// the button that had focus is disabled while it works: move focus to the result
			pending = false;
			await tick();
			ready?.focus();
		} catch (e) {
			if (!mounted || (e instanceof ApiError && e.code === 'step_up_required')) return;
			if (e instanceof ApiError && e.code === 'ice_picks') await readOptions();
			problem = errorKey(e);
			// too long: the list where records can be left out
			if (problem === 'ice_full') shownOpen = true;
		} finally {
			if (mounted) pending = false;
		}
	}
</script>

<section class="card ice" aria-labelledby="ice-title">
	<h3 id="ice-title">{t('ice_title')}</h3>
	<p class="dim">{t('ice_about')}</p>
	{#if options}
		<form onsubmit={(e) => { e.preventDefault(); void issue(); }}>
			<fieldset disabled={pending}>
				<legend>{t('ice_contacts')}</legend>
				{#each contacts as contact, index (contact)}
					<div class="contact">
						<label>{t('ice_name')}<input maxlength="50" bind:value={contact.name} /></label>
						<label>{t('ice_phone')}<input type="tel" maxlength="32" bind:value={contact.phone} /></label>
						{#if contacts.length > 1}
							<button class="btn sm" type="button" onclick={() => contacts.splice(index, 1)}>{t('ice_remove')}</button>
						{/if}
					</div>
				{/each}
				{#if contacts.length < 3}
					<button class="btn sm" type="button" onclick={() => contacts.push({ name: '', phone: '' })}>{t('ice_add')}</button>
				{/if}
			</fieldset>
			<label class="days">{t('ice_lang')}
				<select name="ice-lang" bind:value={lang} disabled={pending}>
					{#each LANGS as l (l.code)}<option value={l.code}>{l.name}</option>{/each}
				</select>
			</label>
			{#if plainItems.length}
				<details bind:open={shownOpen}>
					<summary>{t('ice_shown')} ({plainItems.length - leftOut.length}/{plainItems.length})</summary>
					<p class="dim">{t('ice_shown_d')}</p>
					{#each plainItems as item (item.id)}
						<label class="choice">
							<input type="checkbox" checked={!leftOut.includes(item.id)} onchange={(e) => keep(item.id, e.currentTarget.checked)} disabled={pending} />
							<span>{item.name} <small class="dim">{t('ice_kind_' + item.kind)}</small></span>
						</label>
					{/each}
				</details>
			{/if}
			{#if sensitiveItems.length}
				<details>
					<summary>{t('ice_sensitive')}</summary>
					<p class="dim">{t('ice_sensitive_d')}</p>
					{#each sensitiveItems as item (item.id)}
						<label class="choice">
							<input type="checkbox" value={item.id} bind:group={selected} disabled={pending} />
							<span>{item.name} <small class="dim">{t('ice_kind_' + item.kind)}</small></span>
						</label>
					{/each}
				</details>
			{/if}
			{#if options.online}
				<label class="choice"><input type="checkbox" bind:checked={online} disabled={pending} />{t('ice_online')}</label>
				{#if online}<p class="dim">{t('ice_online_d')}</p>{/if}
			{/if}
			<label class="days">{t('ice_days')}<input type="number" min="1" max="365" required bind:value={days} disabled={pending} aria-describedby="ice-until" /></label>
			<p class="dim" id="ice-until">{until ? tf('ice_until', { date: day(until) }) : ''}</p>
			<p class="dim">{t('ice_replace')}</p>
			<div><button class="btn pri" disabled={pending} aria-busy={pending}>{t(pending ? 'ice_making' : 'ice_create')}</button></div>
		</form>
	{:else if !problem}
		<p class="dim" role="status">{t('loading')}</p>
	{/if}
	{#if problem}<p class="problem" role="alert">{t(problem)}</p>{/if}
	{#if files}
		<div class="result">
			<div role="status" tabindex="-1" bind:this={ready}>{t('ice_ready')} · {tf('ice_until', { date: day(files.expires_at) })}</div>
			<p class="dim">{t('ice_keep')}</p>
			<div class="actions">
				<a class="btn" href={files.pdf} download="ownhealth-ice.pdf">{t('ice_pdf')}</a>
				<a class="btn" href={files.png} download="ownhealth-ice.png">{t('ice_png')}</a>
				<a class="btn" href="/links">{t('ice_manage')}</a>
			</div>
		</div>
	{/if}
</section>

<style>
	.ice { margin-top: 14px; max-width: 50rem; }
	p { margin: 6px 0; }
	form, fieldset { display: grid; gap: 9px; }
	fieldset { padding: 0; margin: 0; border: 0; min-width: 0; }
	fieldset legend { padding-bottom: 5px; }
	fieldset > button { justify-self: start; }
	.contact {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr)) auto;
		gap: 8px;
		align-items: end;
	}
	.contact label, .days { display: grid; gap: 4px; }
	.contact input, .days input, .days select {
		width: 100%;
		padding: 7px 9px;
		color: var(--text);
		background: var(--bg);
		border: 1px solid var(--line);
		border-radius: 5px;
	}
	.days { width: max-content; }
	.days input { max-width: 6rem; }
	.result [role='status']:focus { outline: 2px solid var(--accent); outline-offset: 2px; }
	details summary { cursor: pointer; }
	.choice { display: flex; align-items: baseline; gap: 7px; overflow-wrap: anywhere; }
	.choice input { accent-color: var(--accent); }
	.problem { color: var(--out); }
	.result { margin-top: 12px; padding-top: 10px; border-top: 1px solid var(--line); }
	.actions { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 8px; }
	@media (max-width: 520px) { .contact { grid-template-columns: 1fr; } }
</style>
