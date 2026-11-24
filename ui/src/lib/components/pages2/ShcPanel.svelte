<script lang="ts">
	// SMART Health Card receive (slice 22): pasted shc:/ text, a .smart-health-card file or a photo of
	// the QR code. The server fetches an issuer's keys only for a host the person said yes to here, and
	// a verified card shows who signed it ("signed by <iss>"), never that the signer deserves trust.
	// A SMART Health Link (slice 23) is pasted here too: its host is shown and approved before the
	// server fetches anything, and its passcode is typed once and sent only with that request.
	import { tick } from 'svelte';
	import { receiveCard, type ShcAnswer, type ShcHost, type ShcSource, type ShlExtra, type ShlHeld, type ShlLink } from '#lib/api/shc';
	import { t } from '#lib/i18n/index.svelte';
	import { errRaw, errText, tf } from '#lib/components/pages1/util.svelte';
	import { day } from '#lib/components/pages2/fmt';
	import { declinedKey, identityWarning, isLink, linkRefusalKey, linkRefusalVars, linkWarning, notImportedKey, passcodeRetry, receivedBefore, refusalKey, refusalVars } from '#lib/logic/shc';

	let { profile, profileName, ondone }: { profile: string; profileName: string; ondone: (documentId: string) => void } = $props();

	let text = $state('');
	let source = $state<ShcSource | null>(null);
	let asking = $state<ShcHost[]>([]);
	let approved = $state<string[]>([]);
	let answer = $state<ShcAnswer | null>(null);
	let declined = $state<string | null>(null);
	let error = $state<{ text: string; raw: string } | null>(null);
	let busy = $state(false);
	let fileEl: HTMLInputElement | undefined = $state();
	// the link being asked about, the files an earlier answer stored, and the passcode as typed
	let link = $state<ShlLink | null>(null);
	let held = $state<ShlHeld[]>([]);
	let passcode = $state('');
	// a wrong passcode, shown inside the link's question while the link still allows attempts
	let retry = $state<string | null>(null);
	let panel: HTMLElement | undefined = $state();

	// after an answer, focus goes to the passcode field when it is asked, else to the first question or result
	async function focusNext() {
		await tick();
		const next = panel?.querySelector<HTMLElement>('#shl-passcode') ?? panel?.querySelector<HTMLElement>('[data-focus]');
		next?.focus();
	}

	async function send(s: ShcSource, approve: string[], extra: ShlExtra = {}) {
		if (busy) return;
		busy = true;
		source = s;
		answer = null;
		declined = null;
		error = null;
		retry = null;
		const asked = asking;
		asking = [];
		try {
			const res = await receiveCard(profile, s, approve, extra);
			approved = approve;
			if (res.status === 'needs_approval') {
				asking = res.hosts;
				link = res.link ?? null;
				held = res.held ?? [];
			} else if (res.status === 'refused' && res.kind === 'link' && link && held.length === 0 && passcodeRetry(res.reason, res.detail)) {
				// the same question again, with the refusal in it: the person types the passcode once more
				asking = asked;
				retry = refusal(res);
			} else answer = res;
			if (res.status === 'imported' && res.document_id) ondone(res.document_id);
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
		focusNext();
	}

	function approve(host: string) {
		if (!source) return;
		// the passcode is sent once, with the request that opens the link, and then forgotten
		const extra: ShlExtra = held.length ? { held } : link?.passcode ? { passcode } : {};
		passcode = '';
		send(source, approved.includes(host) ? approved : [...approved, host], extra);
	}

	function read() {
		link = null;
		held = [];
		send({ text }, []);
	}

	function decline(host: string) {
		asking = [];
		retry = null;
		declined = host;
		focusNext();
	}

	function picked(e: Event) {
		const input = e.currentTarget as HTMLInputElement;
		const f = input.files?.[0];
		input.value = '';
		link = null;
		held = [];
		if (f) send({ file: f }, []);
	}

	function submitLink(e: SubmitEvent, host: string) {
		e.preventDefault();
		if (!busy && !(link?.passcode && !passcode)) approve(host);
	}

	const refusal = (a: ShcAnswer & { status: 'refused' }) =>
		a.kind === 'link' ? tf(linkRefusalKey(a.reason, a.detail), linkRefusalVars(a.reason, a.detail, t)) : tf(refusalKey(a.reason), refusalVars(a.reason, a.detail, a.iss, t));
	// the link's host, asked about before anything is fetched; issuer hosts come after, with held files
	const askingLink = $derived(link !== null && held.length === 0);
	// a card that names someone else, or no one we could match, says so before its rows are accepted
	const warning = $derived.by(() => {
		if (answer?.status !== 'imported') return null;
		// a link warns only about files it imported; its FHIR files carry no name to show
		const w = answer.link ? linkWarning(answer.identity, answer.cards, answer.documents ?? [], profileName) : identityWarning(answer.identity, answer.cards, profileName);
		return w ? tf(w.key, w.vars) : null;
	});
</script>

<section class="card shc" aria-label={t('shc_title')} bind:this={panel}>
	<h3>{tf('shc_t', { name: profileName })}<span class="r">{t('shc_kinds')}</span></h3>
	<label class="dim" for="shc-text" style="font-size:11.5px">{t('shc_paste_l')}</label>
	<textarea id="shc-text" class="mono" rows="3" bind:value={text} placeholder="shc:/5676290952432060346029243740446031… · shlink:/eyJ1cmwiOi…" disabled={busy}></textarea>
	<div class="p2row" style="margin-top:8px">
		<button class="btn pri" onclick={read} disabled={busy || !text.trim()}>{isLink(text) ? t('shlr_read') : t('shc_read')}</button>
		<button class="btn" onclick={() => fileEl?.click()} disabled={busy}>{t('shc_file')}</button>
		<input bind:this={fileEl} type="file" hidden accept=".smart-health-card,application/smart-health-card,.json,image/png,image/jpeg" onchange={picked} />
	</div>
	<div class="note" style="margin-top:8px"><span>{t('shc_note')}</span></div>
	{#if busy}<div class="muted" role="status" style="margin-top:8px">{t('shc_busy')}</div>{/if}
	{#if error}<div class="p2err" role="alert" style="margin-top:8px" title={error.raw || undefined} tabindex="-1" data-focus>{error.text}</div>{/if}

	{#if askingLink && link}
		{#each asking as i (i.host)}
			<form class="ask" aria-label={t('shlr_ask_t')} tabindex="-1" data-focus onsubmit={(e) => submitLink(e, i.host)}>
				<div style="font-weight:600">{t('shlr_ask_t')}</div>
				{#if link.label}<div class="muted">{t('shlr_ask_label')} <span class="mono">{link.label}</span></div>{/if}
				<div class="muted" style="margin-top:4px">{tf('shlr_ask_d', { host: i.host })}</div>
				{#if link.expires}<div class="dim" style="font-size:11.5px">{tf('shlr_ask_expires', { date: day(link.expires) })}</div>{/if}
				{#if retry}<div class="p2err" role="alert" style="margin-top:8px">{retry}</div>{/if}
				{#if link.passcode}
					<label class="dim" for="shl-passcode" style="font-size:11.5px;display:block;margin-top:8px">{t('shlr_passcode_l')}</label>
					<input id="shl-passcode" type="password" autocomplete="off" bind:value={passcode} disabled={busy} />
				{/if}
				<div class="p2row" style="margin-top:8px">
					<button class="btn pri" type="submit" disabled={busy || (link.passcode && !passcode)}>{tf('shlr_approve', { host: i.host })}</button>
					<button class="btn" type="button" onclick={() => decline(i.host)} disabled={busy}>{t('shc_decline')}</button>
				</div>
			</form>
		{/each}
	{/if}
	{#each askingLink ? [] : asking as i (i.host)}
		<div class="ask" role="group" aria-label={t('shc_ask_t')} tabindex="-1" data-focus>
			<div style="font-weight:600">{t('shc_ask_t')}</div>
			<div class="muted">{t('shc_ask_signed')}</div>
			{#each i.issuers as iss (iss)}<div class="mono" style="font-size:12px">{iss}</div>{/each}
			<div class="muted" style="margin-top:4px">{tf('shc_ask_d', { host: i.host })}</div>
			<div class="p2row" style="margin-top:8px">
				<button class="btn pri" onclick={() => approve(i.host)} disabled={busy}>{tf('shc_approve', { host: i.host })}</button>
				<button class="btn" onclick={() => decline(i.host)} disabled={busy}>{t('shc_decline')}</button>
			</div>
		</div>
	{/each}

	{#if declined}<div class="p2err" role="alert" style="margin-top:8px" tabindex="-1" data-focus>{tf(declinedKey(held.length), { host: declined })}</div>{/if}
	{#if answer?.status === 'refused'}
		<div class="p2err" role="alert" style="margin-top:8px" tabindex="-1" data-focus>{refusal(answer)}</div>
	{/if}
	{#if answer?.status === 'imported'}
		{#if warning}<div class="p2err" role="alert" style="margin-top:8px" tabindex="-1" data-focus>{warning}</div>{/if}
		<div class="done" role="status" tabindex="-1" data-focus>
			{#if answer.link}<div class="dim" style="font-size:11.5px">{tf('shlr_from', { host: answer.link.host })}{#if answer.link.label}{' · '}<span class="mono">{answer.link.label}</span>{/if}</div>{/if}
			{#each answer.cards as c, n (n)}
				<div><span class="chip">{t('shc_signed_by')}</span> <span class="mono">{c.iss}</span></div>
				<div class="dim" style="font-size:11.5px">{tf('shc_issued', { date: day(c.issued) })}{#if c.expires} · {tf('shc_expires', { date: day(c.expires) })}{/if}</div>
			{/each}
			<div style="margin-top:6px">{tf('shc_summary', { accepted: answer.summary.accepted, queued: answer.summary.queued, skipped: answer.summary.skipped })}</div>
			{#if answer.link && receivedBefore(answer.documents)}<div class="dim" style="font-size:11.5px">{t('shlr_again')}</div>{/if}
			{#each answer.not_imported ?? [] as n, k (k)}
				<div class="dim" style="font-size:11.5px">{tf(notImportedKey(n.reason), { type: n.content_type })}</div>
			{/each}
			<div class="p2row" style="margin-top:6px;font-size:12px">
				<a href="/timeline">{t('shc_open_timeline')}</a>
				{#if answer.document_id}<a href="?doc={answer.document_id}">{answer.link ? t('shlr_open_doc') : t('shc_open_doc')}</a>{/if}
			</div>
		</div>
	{/if}
</section>

<style>
	.shc {
		margin-bottom: 12px;
		border-style: dashed;
		border-color: var(--line2);
	}
	textarea {
		width: 100%;
		box-sizing: border-box;
		margin-top: 4px;
		background: var(--bg2, #091017);
		color: var(--text);
		border: 1px solid var(--line2);
		border-radius: 6px;
		padding: 6px 8px;
		resize: vertical;
		word-break: break-all;
	}
	.ask,
	.done {
		margin-top: 10px;
		padding: 10px 12px;
		border: 1px solid var(--line2);
		border-radius: 8px;
		overflow-wrap: anywhere;
	}
	.ask {
		border-color: rgba(224, 168, 74, 0.55);
	}
	/* the list of kinds wraps under the title on a phone instead of running off the card */
	h3 .r {
		white-space: normal;
	}
	.ask input {
		width: 100%;
		max-width: 260px;
		box-sizing: border-box;
		margin-top: 4px;
		background: var(--bg2, #091017);
		color: var(--text);
		border: 1px solid var(--line2);
		border-radius: 6px;
		padding: 6px 8px;
	}
	/* a long host wraps inside the card on a phone instead of running off it */
	.ask .btn {
		white-space: normal;
		overflow-wrap: anywhere;
		text-align: left;
		max-width: 100%;
	}
</style>
