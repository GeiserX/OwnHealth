<script lang="ts">
	// Log in, or set up the first owner on a fresh install. Passkeys where the address allows them
	// (https or localhost); the password fallback only on a plain-HTTP LAN address; a recovery code
	// when the passkey is lost. After setup the recovery codes are shown once.
	import '#lib/components/pages2/pages2.css';
	import { getAuthState, loginBegin, loginFinish, passwordLogin, recoveryLogin, setupBegin, setupFinish, setupPassword } from '#lib/api';
	import type { AuthState } from '#lib/api/types';
	import { Loader } from '#lib/components/pages1/util.svelte';
	import { i18n, setLang, t } from '#lib/i18n/index.svelte';
	import { createPasskey, usePasskey } from './webauthn';

	let { ondone = () => location.reload() }: { ondone?: () => void } = $props();

	const st = new Loader<AuthState>();
	$effect(() => {
		st.run(getAuthState);
	});
	let mode = $state<'main' | 'recovery'>('main');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let codes = $state<string[] | null>(null);
	let f = $state({ household_name: '', display_name: '', birth_date: '', sex_at_birth: '', country: 'ES', password: '' });
	let login = $state({ name: '', password: '', code: '' });

	async function run(fn: () => Promise<void>) {
		busy = true;
		error = null;
		try {
			await fn();
		} catch (e) {
			error = e instanceof Error && e.message !== 'cancelled' ? e.message : t('i2_cancelled');
		} finally {
			busy = false;
		}
	}
	const setupBody = () => ({
		household_name: f.household_name.trim(),
		display_name: f.display_name.trim(),
		locale: i18n.lang,
		birth_date: f.birth_date || null,
		sex_at_birth: f.sex_at_birth || null,
		country: f.country.trim().toUpperCase() || null
	});
	function setup(e: SubmitEvent) {
		e.preventDefault();
		run(async () => {
			const res = st.data?.passkeys_possible
				? await setupFinish(await createPasskey(await setupBegin(setupBody())), navigator.platform || undefined)
				: await setupPassword({ ...setupBody(), password: f.password });
			codes = res.recovery_codes;
		});
	}
	const withPasskey = () => run(async () => {
		await loginFinish(await usePasskey(await loginBegin()));
		ondone();
	});
	function withPassword(e: SubmitEvent) {
		e.preventDefault();
		run(async () => {
			await passwordLogin(login.name, login.password);
			ondone();
		});
	}
	function withCode(e: SubmitEvent) {
		e.preventDefault();
		run(async () => {
			await recoveryLogin(login.code.trim());
			ondone();
		});
	}
	const s = $derived(st.data);
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

	{#if st.error}
		<div class="card"><div class="p2err" role="alert">{st.error.message}</div></div>
	{:else if !s}
		<div class="empty">{t('loading')}</div>
	{:else if codes}
		<section class="card p2form" aria-labelledby="rc-h">
			<h3 id="rc-h">{t('i2_rc_t')}</h3>
			<div class="muted">{t('i2_rc_setup')}</div>
			<div class="p2secret" style="columns:2">{#each codes as c (c)}<div>{c}</div>{/each}</div>
			<div class="note" style="color:var(--watch)">{t('i2_rc_once')}</div>
			<button class="btn pri" onclick={ondone}>{t('i2_continue')}</button>
		</section>
	{:else if s.setup_needed}
		<form class="card p2form" onsubmit={setup} aria-labelledby="su-h">
			<h3 id="su-h">{t('i2_setup_t')}</h3>
			<div class="muted">{t('i2_setup_d')}</div>
			<label>{t('i2_your_name')}<input class="p2in" bind:value={f.display_name} required maxlength="80" autocomplete="name" /></label>
			<label>{t('i2_hh_name')}<input class="p2in" bind:value={f.household_name} maxlength="80" placeholder={t('i2_hh_ph')} /></label>
			<div class="p2row">
				<label style="flex:1">{t('i2_birth')}<input class="p2in" type="date" bind:value={f.birth_date} /></label>
				<label>{t('i2_sex')}
					<select class="p2in" bind:value={f.sex_at_birth}>
						<option value="">—</option><option value="female">{t('i2_sex_f')}</option><option value="male">{t('i2_sex_m')}</option><option value="unknown">{t('i2_sex_u')}</option>
					</select>
				</label>
				<label>{t('i2_country')}<input class="p2in" bind:value={f.country} maxlength="2" style="width:56px" /></label>
			</div>
			{#if !s.passkeys_possible}
				<label>{t('i2_pw')}<input class="p2in" type="password" bind:value={f.password} required minlength="12" autocomplete="new-password" /></label>
				<div class="note">{t('i2_pw_http')}</div>
			{/if}
			{#if error}<div class="p2err" role="alert">{error}</div>{/if}
			<button class="btn pri" type="submit" disabled={busy}>{s.passkeys_possible ? t('i2_setup_pk') : t('i2_setup_pw')}</button>
			<div class="note">{t('i2_setup_note')}</div>
		</form>
	{:else if s.user && s.auth_kind}
		<section class="card p2form">
			<h3>{t('i2_signed_in')} {s.user.display_name}</h3>
			<a class="btn pri" href="/">{t('nav_dash')}</a>
		</section>
	{:else if mode === 'recovery'}
		<form class="card p2form" onsubmit={withCode} aria-labelledby="rec-h">
			<h3 id="rec-h">{t('i2_rc_use')}</h3>
			<div class="muted">{t('i2_rc_use_d')}</div>
			<label>{t('i2_rc_code')}<input class="p2in mono" bind:value={login.code} required autocomplete="one-time-code" /></label>
			{#if error}<div class="p2err" role="alert">{error}</div>{/if}
			<button class="btn pri" type="submit" disabled={busy}>{t('login')}</button>
			<button class="btn" type="button" onclick={() => (mode = 'main')}>{t('i2_back')}</button>
		</form>
	{:else}
		<section class="card p2form" aria-labelledby="li-h">
			<h3 id="li-h">{t('login')}</h3>
			{#if s.passkeys_possible}
				<button class="btn pri" onclick={withPasskey} disabled={busy}>{t('i2_login_pk')}</button>
			{/if}
			{#if s.password_allowed}
				<form class="p2form" onsubmit={withPassword}>
					<label>{t('i2_your_name')}<input class="p2in" bind:value={login.name} required autocomplete="username" /></label>
					<label>{t('i2_pw')}<input class="p2in" type="password" bind:value={login.password} required autocomplete="current-password" /></label>
					<button class="btn pri" type="submit" disabled={busy}>{t('login')}</button>
				</form>
			{/if}
			{#if s.oidc.enabled}<a class="btn" href="/api/auth/oidc/login">{s.oidc.label ?? 'OIDC'}</a>{/if}
			{#if error}<div class="p2err" role="alert">{error}</div>{/if}
			<button class="btn" onclick={() => (mode = 'recovery')}>{t('i2_rc_use')}</button>
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
