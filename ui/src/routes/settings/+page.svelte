<script lang="ts">
	// Settings: your passkeys and recovery codes, and for each profile you manage its locks, the two
	// sensitive-genome switches, and who has access (grants). Changes that widen access need step-up.
	import '#lib/components/pages2/pages2.css';
	import {
		addPasskeyBegin,
		addPasskeyFinish,
		deleteGrant,
		getAuthState,
		getHousehold,
		listPasskeys,
		logout,
		newRecoveryCodes,
		putGrant,
		putLocks,
		putSensitive,
		removePasskey
	} from '#lib/api';
	import type { AuthState, Household, PasskeyOut, Role, Scope } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import Switch from '#lib/components/pages2/Switch.svelte';
	import { ago, day } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { createPasskey } from '#lib/components/pages2/webauthn';
	import { t } from '#lib/i18n/index.svelte';

	const LOCKABLE: Scope[] = ['read:labs', 'read:vitals', 'read:wearables', 'read:records', 'read:genome', 'read:genome_raw', 'read:sensitive', 'read:documents_text'];
	const ROLES: Role[] = ['viewer', 'caregiver', 'guardian'];

	const auth = new Loader<AuthState>();
	const keys = new Loader<PasskeyOut[]>();
	const hh = new Loader<Household>();
	let codes = $state<string[] | null>(null);
	let error = $state<string | null>(null);
	let busy = $state(false);

	const loadAll = () => {
		auth.run(getAuthState);
		keys.run(async () => (await listPasskeys()).items);
		hh.run(getHousehold);
	};
	$effect(loadAll);

	async function act(fn: () => Promise<unknown>, reload = true) {
		busy = true;
		error = null;
		try {
			await withStepUp(fn);
			if (reload) loadAll();
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
	const addKey = () => act(async () => addPasskeyFinish(await createPasskey(await addPasskeyBegin()), navigator.platform || undefined));
	const regen = () => act(async () => (codes = (await newRecoveryCodes()).recovery_codes), false);
	async function signOut() {
		await logout().catch(() => null);
		location.href = '/';
	}
	const managed = $derived((hh.data?.profiles ?? []).filter((p) => p.can_manage));
	const member = (id: string) => hh.data?.members.find((m) => m.user_id === id)?.display_name ?? id.slice(0, 8);
	const me = $derived(hh.data?.members.find((m) => m.is_me)?.user_id);
	const editableGrants = $derived((hh.data?.grants ?? []).filter((g) => g.role !== 'owner' && g.user_id !== me && managed.some((p) => p.slug === g.profile)));
	function toggleLock(slug: string, locks: Scope[], s: Scope, on: boolean) {
		const next = on ? [...locks, s] : locks.filter((x) => x !== s);
		act(() => putLocks(slug, next));
	}
</script>

<div class="ph">
	<div>
		<h1>{t('i2_settings')}</h1>
		<div class="sub">{t('i2_set_sub')}</div>
	</div>
	<div class="right"><a class="btn" href="/mcp-access">{t('nav_mcp')}</a><button class="btn" onclick={signOut}>{t('i2_logout')}</button></div>
</div>

{#if error}<div class="p2err" role="alert" style="margin-bottom:8px">{error}</div>{/if}

<div class="cols">
	<div class="grid" style="align-content:start">
		<section class="card">
			<h3>{t('i2_passkeys')}<span class="r">{auth.data?.passkeys_possible ? '' : t('i2_pk_http')}</span></h3>
			{#if keys.error}
				<NotReady error={keys.error} />
			{:else}
				<table class="t">
					<tbody>
						{#each keys.data ?? [] as k (k.id)}
							<tr>
								<td><b>{k.label ?? t('i2_pk_unnamed')}</b><div class="dim mono" style="font-size:10.5px">{k.id.slice(0, 12)}…</div></td>
								<td class="muted">{t('i2_added')} {day(k.created_at)}</td>
								<td class="muted">{t('i2_t_last')}: {ago(k.last_used_at)}</td>
								<td style="text-align:right"><button class="btn sm no" disabled={busy} onclick={() => act(() => removePasskey(k.id))}>{t('i2_remove')}</button></td>
							</tr>
						{:else}
							<tr><td><div class="empty">{t('i2_pk_none')}</div></td></tr>
						{/each}
					</tbody>
				</table>
			{/if}
			<div class="p2row" style="margin-top:8px">
				<button class="btn" onclick={addKey} disabled={busy || !auth.data?.passkeys_possible}>{t('i2_pk_add')}</button>
				<span class="dim" style="font-size:11.5px">{t('i2_pk_add_d')}</span>
			</div>
		</section>
		<section class="card">
			<h3>{t('i2_rc')}<span class="r">{auth.data?.user ? tf('i2_rc_left', { n: auth.data.user.recovery_codes_left }) : ''}</span></h3>
			<div class="muted" style="font-size:12px">{t('i2_rc_d')}</div>
			{#if codes}
				<div class="p2secret" style="margin-top:8px;columns:2" aria-live="polite">{#each codes as c (c)}<div>{c}</div>{/each}</div>
				<div class="note" style="margin-top:6px;color:var(--watch)">{t('i2_rc_once')}</div>
			{/if}
			<div class="p2row" style="margin-top:8px"><button class="btn" onclick={regen} disabled={busy}>{t('i2_rc_new')}</button></div>
		</section>
		<section class="card" style="padding:8px 6px 2px">
			<h3 style="padding:0 8px">{t('i2_grants')}<span class="r">{t('i2_grants_d')}</span></h3>
			{#if hh.error}
				<NotReady error={hh.error} />
			{:else}
				<table class="t">
					<thead><tr><th>{t('i2_person')}</th><th>{t('profile')}</th><th>{t('i2_role')}</th><th>{t('i2_t_exp')}</th><th></th></tr></thead>
					<tbody>
						{#each editableGrants as g (g.user_id + g.profile)}
							<tr>
								<td>{member(g.user_id)}</td>
								<td>{g.profile}</td>
								<td>
									<select class="p2in" value={g.role} aria-label="{t('i2_role')} {member(g.user_id)} · {g.profile}" disabled={busy} onchange={(e) => act(() => putGrant({ user_id: g.user_id, profile: g.profile, role: (e.currentTarget as HTMLSelectElement).value as Role }))}>
										{#each ROLES as r (r)}<option value={r}>{t('r_' + r)}</option>{/each}
									</select>
								</td>
								<td class="muted">{g.expires_at ? day(g.expires_at) : t('i2_never')}</td>
								<td style="text-align:right"><button class="btn sm no" disabled={busy} onclick={() => act(() => deleteGrant(g.user_id, g.profile))}>{t('i2_revoke')}</button></td>
							</tr>
						{:else}
							<tr><td colspan="5"><div class="empty">{t('i2_grants_none')}</div></td></tr>
						{/each}
					</tbody>
				</table>
				<div class="note" style="padding:6px 8px">{t('i2_grants_note')} <a href="/family">{t('nav_fam')}</a></div>
			{/if}
		</section>
	</div>

	<div class="grid" style="align-content:start">
		{#each managed as p (p.id)}
			<section class="card">
				<h3>{p.display_name}<span class="r">{t('r_' + p.role)}{p.is_minor ? ' · ' + t('i2_minor') : ''}</span></h3>
				<div class="dim" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em;margin-bottom:2px">{t('i2_sensitive')}</div>
				<div class="set">
					<Switch on={p.genome_sensitive_ui} label="{t('i2_sens_ui')} · {p.display_name}" disabled={busy || (p.is_minor && !p.genome_sensitive_ui)} onchange={(on) => act(() => putSensitive(p.slug, { ui: on }))} />
					<div style="flex:1"><div>{t('i2_sens_ui')}</div><div class="dim" style="font-size:11px">{t('i2_sens_ui_d')}</div></div>
				</div>
				<div class="set">
					<Switch on={!!p.genome_sensitive_agents} label="{t('i2_sens_ag')} · {p.display_name}" disabled={busy || (p.is_minor && !p.genome_sensitive_agents)} onchange={(on) => act(() => putSensitive(p.slug, { agents: on }))} />
					<div style="flex:1"><div>{t('i2_sens_ag')}</div><div class="dim" style="font-size:11px">{t('i2_sens_ag_d')}</div></div>
				</div>
				{#if p.is_minor}<div class="note">{t('i2_minor_d')}</div>{/if}
				<div class="dim" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em;margin:10px 0 2px">{t('i2_locks')}</div>
				<div class="note" style="margin-bottom:4px">{t('i2_locks_d')}</div>
				{#each LOCKABLE as s (s)}
					{@const locked = (p.locks ?? []).includes(s)}
					<div class="set">
						<Switch on={locked} label="{t('i2_lock')} {s} · {p.display_name}" disabled={busy} onchange={(on) => toggleLock(p.slug, p.locks ?? [], s, on)} />
						<span class="mono" style="flex:1">{s}</span>
						<span class="dim" style="font-size:11px">{locked ? t('i2_locked_w') : ''}</span>
					</div>
				{/each}
			</section>
		{:else}
			{#if hh.data}<section class="card"><div class="empty">{t('i2_manage_none')}</div></section>{/if}
		{/each}
	</div>
</div>

<StepUpDialog />

<style>
	.cols {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 400px;
		gap: 14px;
	}
</style>
