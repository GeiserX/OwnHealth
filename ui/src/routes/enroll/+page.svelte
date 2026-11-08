<script lang="ts">
	// The page an invite link opens (/enroll?code=…): who the invite is for, then a passkey (or the
	// password fallback on a plain-HTTP LAN address, or OIDC), then the recovery codes once.
	import '#lib/components/pages2/pages2.css';
	import { page } from '$app/state';
	import { enrollBegin, enrollFinish, enrollPassword, getInvite, type InviteInfo } from '#lib/api';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import { createPasskey } from '#lib/components/pages2/webauthn';
	import { i18n, setLang, t } from '#lib/i18n/index.svelte';

	const code = $derived(page.url.searchParams.get('code') ?? '');
	const inv = new Loader<InviteInfo>();
	$effect(() => {
		const c = code;
		if (c) inv.run(() => getInvite(c));
	});
	let pw = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let codes = $state<string[] | null>(null);

	async function run(fn: () => Promise<{ recovery_codes: string[] }>) {
		busy = true;
		error = null;
		try {
			codes = (await fn()).recovery_codes;
		} catch (e) {
			error = e instanceof Error && e.message !== 'cancelled' ? e.message : t('i2_cancelled');
		} finally {
			busy = false;
		}
	}
	const withPasskey = () => run(async () => enrollFinish(code, await createPasskey(await enrollBegin(code)), navigator.platform || undefined));
	function withPassword(e: SubmitEvent) {
		e.preventDefault();
		run(() => enrollPassword(code, pw));
	}
	const s = $derived(inv.data);
</script>

<div class="login">
	<div class="brand" style="justify-content:center">
		<div class="logo"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="#6ea8fe" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 12h4l2-5 4 10 2-5h6" /></svg></div>
		<div>OwnHealth<small>{t('selfhosted')} · v0.1</small></div>
	</div>
	<div class="seg" role="group" aria-label="Language" style="align-self:center">
		<button class:on={i18n.lang === 'es'} onclick={() => setLang('es')}>ES</button>
		<button class:on={i18n.lang === 'en'} onclick={() => setLang('en')}>EN</button>
	</div>

	{#if !code}
		<div class="card"><div class="p2err" role="alert">{t('en_nocode')}</div></div>
	{:else if inv.error}
		<div class="card"><div class="p2err" role="alert">{t('en_invalid')}</div></div>
	{:else if !s}
		<div class="empty">{t('loading')}</div>
	{:else if codes}
		<section class="card p2form" aria-labelledby="rc-h">
			<h3 id="rc-h">{t('i2_rc_t')}</h3>
			<div class="muted">{t('i2_rc_setup')}</div>
			<div class="p2secret" style="columns:2" data-testid="recovery-codes">{#each codes as c (c)}<div>{c}</div>{/each}</div>
			<div class="note" style="color:var(--watch)">{t('i2_rc_once')}</div>
			<a class="btn pri" href="/">{t('i2_continue')}</a>
		</section>
	{:else}
		<section class="card p2form" aria-labelledby="en-h">
			<h3 id="en-h">{tf('en_t', { name: s.display_name })}</h3>
			<div class="muted">{tf('en_d', { hh: s.household })}{s.claims_profile ? ' ' + t('en_claims') : ''}</div>
			{#if s.passkeys_possible}
				<button class="btn pri" onclick={withPasskey} disabled={busy}>{t('en_pk')}</button>
			{/if}
			{#if s.password_allowed}
				<form class="p2form" onsubmit={withPassword}>
					<label>{t('i2_pw')}<input class="p2in" type="password" bind:value={pw} required minlength="12" autocomplete="new-password" /></label>
					<div class="note">{t('i2_pw_http')}</div>
					<button class="btn pri" type="submit" disabled={busy}>{t('en_pw')}</button>
				</form>
			{/if}
			{#if s.oidc}<a class="btn" href="/api/auth/oidc/login?mode=enroll&invite={encodeURIComponent(code)}">OIDC</a>{/if}
			{#if error}<div class="p2err" role="alert">{error}</div>{/if}
		</section>
	{/if}
	<div class="dim" style="font-size:11px;text-align:center">{t('nodx')} · <span class="mono">{t('agpl')}</span></div>
</div>

<style>
	.login {
		max-width: 420px;
		margin: 6vh auto 0;
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.login .card {
		padding: 16px 18px;
	}
	.login .btn {
		justify-content: center;
		padding: 7px 12px;
	}
</style>
