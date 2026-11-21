<script lang="ts">
	// SMART Health Card receive (slice 22): pasted shc:/ text, a .smart-health-card file or a photo of
	// the QR code. The server fetches an issuer's keys only for a host the person said yes to here, and
	// a verified card shows who signed it ("signed by <iss>"), never that the signer deserves trust.
	import { receiveCard, type ShcAnswer, type ShcHost, type ShcSource } from '#lib/api/shc';
	import { t } from '#lib/i18n/index.svelte';
	import { errRaw, errText, tf } from '#lib/components/pages1/util.svelte';
	import { day } from '#lib/components/pages2/fmt';
	import { identityWarning, refusalKey, refusalVars } from '#lib/logic/shc';

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

	async function send(s: ShcSource, approve: string[]) {
		if (busy) return;
		busy = true;
		source = s;
		answer = null;
		declined = null;
		error = null;
		asking = [];
		try {
			const res = await receiveCard(profile, s, approve);
			approved = approve;
			if (res.status === 'needs_approval') asking = res.hosts;
			else answer = res;
			if (res.status === 'imported') ondone(res.document_id);
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}

	function approve(host: string) {
		if (source) send(source, [...approved, host]);
	}

	function decline(host: string) {
		asking = [];
		declined = host;
	}

	function picked(e: Event) {
		const input = e.currentTarget as HTMLInputElement;
		const f = input.files?.[0];
		input.value = '';
		if (f) send({ file: f }, []);
	}

	const refusal = (a: ShcAnswer & { status: 'refused' }) => tf(refusalKey(a.reason), refusalVars(a.reason, a.detail, a.iss, t));
	// a card that names someone else, or no one we could match, says so before its rows are accepted
	const warning = $derived.by(() => {
		if (answer?.status !== 'imported') return null;
		const named = answer.cards.find((c) => c.name) ?? answer.cards[0] ?? { name: null, birth_date: null };
		const w = identityWarning(answer.identity, named, profileName);
		return w ? tf(w.key, w.vars) : null;
	});
</script>

<section class="card shc" aria-label={t('shc_title')}>
	<h3>{tf('shc_t', { name: profileName })}<span class="r">{t('shc_kinds')}</span></h3>
	<label class="dim" for="shc-text" style="font-size:11.5px">{t('shc_paste_l')}</label>
	<textarea id="shc-text" class="mono" rows="3" bind:value={text} placeholder="shc:/5676290952432060346029243740446031…" disabled={busy}></textarea>
	<div class="p2row" style="margin-top:8px">
		<button class="btn pri" onclick={() => send({ text }, [])} disabled={busy || !text.trim()}>{t('shc_read')}</button>
		<button class="btn" onclick={() => fileEl?.click()} disabled={busy}>{t('shc_file')}</button>
		<input bind:this={fileEl} type="file" hidden accept=".smart-health-card,application/smart-health-card,.json,image/png,image/jpeg" onchange={picked} />
	</div>
	<div class="note" style="margin-top:8px"><span>{t('shc_note')}</span></div>
	{#if busy}<div class="muted" role="status" style="margin-top:8px">{t('shc_busy')}</div>{/if}
	{#if error}<div class="p2err" role="alert" style="margin-top:8px" title={error.raw || undefined}>{error.text}</div>{/if}

	{#each asking as i (i.host)}
		<div class="ask" role="group" aria-label={t('shc_ask_t')}>
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

	{#if declined}<div class="p2err" role="alert" style="margin-top:8px">{tf('shc_declined', { host: declined })}</div>{/if}
	{#if answer?.status === 'refused'}
		<div class="p2err" role="alert" style="margin-top:8px" title={answer.message}>{refusal(answer)}</div>
	{/if}
	{#if answer?.status === 'imported'}
		{#if warning}<div class="p2err" role="alert" style="margin-top:8px">{warning}</div>{/if}
		<div class="done" role="status">
			{#each answer.cards as c, n (n)}
				<div><span class="chip">{t('shc_signed_by')}</span> <span class="mono">{c.iss}</span></div>
				<div class="dim" style="font-size:11.5px">{tf('shc_issued', { date: day(c.issued) })}{#if c.expires} · {tf('shc_expires', { date: day(c.expires) })}{/if}</div>
			{/each}
			<div style="margin-top:6px">{tf('shc_summary', { accepted: answer.summary.accepted, queued: answer.summary.queued, skipped: answer.summary.skipped })}</div>
			<div class="p2row" style="margin-top:6px;font-size:12px">
				<a href="/timeline">{t('shc_open_timeline')}</a>
				<a href="?doc={answer.document_id}">{t('shc_open_doc')}</a>
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
		background: var(--bg2, #0b0d11);
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
	/* a long host wraps inside the card on a phone instead of running off it */
	.ask .btn {
		white-space: normal;
		overflow-wrap: anywhere;
		text-align: left;
		max-width: 100%;
	}
</style>
