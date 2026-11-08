<script lang="ts">
	// Family (mockup screen #family): the household's profiles, the profile switcher and the grant matrix.
	// Profiles come from GET /me; the full matrix from GET /household (lane G). Pedigree and hints are
	// v0.3, break-glass v1.0: their cards say so.
	import '#lib/components/pages2/pages2.css';
	import { addRelative, getHousehold, getMe, inviteMember, type RelativeIn } from '#lib/api';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import type { Household, ProfileSummary } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf, tp } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { roleCell } from '#lib/logic/grants';
	import { AVATAR_COLORS, app, currentProfile, setProfile } from '#lib/state/app.svelte';

	const me = $derived(app.me);
	const profiles = $derived(me?.profiles ?? []);
	const onScreen = $derived(currentProfile());
	const hh = new Loader<Household>();

	function load() {
		if (me) hh.run(getHousehold);
	}
	$effect(load);

	const REL = ['self', 'partner', 'child', 'parent'];
	// `relationship` is relative to the household's founder, so 'self' on someone else's profile is not "you".
	const rel = (p: ProfileSummary) =>
		t('h1_rel_' + (p.is_self ? 'self' : p.relationship !== 'self' && REL.includes(p.relationship ?? '') ? p.relationship : 'other'));
	const first = (name: string) => name.split(' ')[0];
	const color = (slug: string) => AVATAR_COLORS[Math.max(0, profiles.findIndex((p) => p.slug === slug)) % AVATAR_COLORS.length];
	// Matrix columns: the household's profiles when the service answers, else the ones I can see.
	const cols = $derived(hh.data?.profiles ?? profiles);
	// Add a person without a login (a child, a parent), or invite someone who gets their own login.
	let form = $state<'' | 'relative' | 'invite'>('');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let nr = $state({ display_name: '', relationship: 'child', role: 'guardian', birth_date: '', sex_at_birth: '', country: 'ES' });
	let inv = $state({ display_name: '', locale: i18n.lang as 'es' | 'en', claim_profile: '' });
	let invited = $state<{ name: string; url: string; code: string; days: number } | null>(null);
	let copied = $state(false);
	const unclaimed = $derived((hh.data?.profiles ?? []).filter((p) => p.can_manage && p.claimed === false));
	const isOwner = $derived(profiles.some((p) => p.role === 'owner'));

	async function act(fn: () => Promise<void>) {
		busy = true;
		error = null;
		try {
			await withStepUp(fn);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
	function saveRelative(e: SubmitEvent) {
		e.preventDefault();
		act(async () => {
			const body: RelativeIn = {
				display_name: nr.display_name.trim(),
				role: nr.role as RelativeIn['role'],
				relationship: nr.relationship as RelativeIn['relationship'],
				birth_date: nr.birth_date || null,
				sex_at_birth: (nr.sex_at_birth || null) as RelativeIn['sex_at_birth'],
				country: nr.country.trim().toUpperCase() || null
			};
			const made = await addRelative(body);
			app.me = await getMe();
			setProfile(made.slug);
			load();
			form = '';
			nr = { ...nr, display_name: '', birth_date: '', sex_at_birth: '' };
		});
	}
	function sendInvite(e: SubmitEvent) {
		e.preventDefault();
		act(async () => {
			const name = inv.display_name.trim();
			const r = await inviteMember({ display_name: name, locale: inv.locale, claim_profile: inv.claim_profile || null });
			invited = { name, url: r.url, code: r.code, days: r.expires_in_days };
			copied = false;
			load();
			form = '';
			inv = { ...inv, display_name: '', claim_profile: '' };
		});
	}
	async function copy(text: string) {
		// No clipboard on a plain-HTTP LAN address: only a write that succeeded says "Copied".
		try {
			await navigator.clipboard.writeText(text);
			copied = true;
		} catch {
			copied = false;
		}
	}
	const manages = (slug: string) => !!hh.data?.profiles.find((p) => p.slug === slug)?.can_manage;
</script>

<div class="ph">
	<div>
		<h1>{t('title_fam')}</h1>
		{#if me}<div class="sub">{tp('h1_fam_sub', cols.length, { hh: me.household.name })}</div>{/if}
	</div>
	<div class="right">
		<PhaseChip phase={t('h1_ph_roles')} /><PhaseChip phase={t('h1_ph_ped')} /><PhaseChip phase={t('h1_ph_bg')} />
		<button class="btn" onclick={() => (form = form === 'relative' ? '' : 'relative')} aria-expanded={form === 'relative'}>{t('fm_add')}</button>
		<button class="btn pri" onclick={() => (form = form === 'invite' ? '' : 'invite')} aria-expanded={form === 'invite'} disabled={!isOwner && !unclaimed.length} title={!isOwner && !unclaimed.length ? t('fm_inv_owner') : undefined}>{t('fm_invite')}</button>
	</div>
</div>

{#if error}<div class="p2err" role="alert" style="margin-bottom:8px">{error}</div>{/if}

{#if form === 'relative'}
	<form class="card p2form" style="margin-bottom:12px" onsubmit={saveRelative} aria-labelledby="fm-rel-h">
		<h3 id="fm-rel-h">{t('fm_add')}<span class="r">{t('fm_add_d')}</span></h3>
		<div class="p2row">
			<label style="flex:1;min-width:180px">{t('fm_name')}<input class="p2in" bind:value={nr.display_name} required maxlength="80" /></label>
			<label>{t('fm_rel')}
				<select class="p2in" bind:value={nr.relationship}>
					{#each ['child', 'parent', 'partner', 'other'] as r (r)}<option value={r}>{t('h1_rel_' + r)}</option>{/each}
				</select>
			</label>
			<label>{t('fm_your_role')}
				<select class="p2in" bind:value={nr.role}>
					<option value="guardian">{t('r_guardian')}</option><option value="caregiver">{t('r_caregiver')}</option>
				</select>
			</label>
		</div>
		<div class="p2row">
			<label>{t('i2_birth')}<input class="p2in" type="date" bind:value={nr.birth_date} /></label>
			<label>{t('i2_sex')}
				<select class="p2in" bind:value={nr.sex_at_birth}>
					<option value="">—</option><option value="female">{t('i2_sex_f')}</option><option value="male">{t('i2_sex_m')}</option><option value="unknown">{t('i2_sex_u')}</option>
				</select>
			</label>
			<label>{t('i2_country')}<input class="p2in" bind:value={nr.country} maxlength="2" style="width:56px" /></label>
		</div>
		<div class="note">{t('fm_role_d')}</div>
		<div class="p2row"><button class="btn pri" type="submit" disabled={busy}>{t('fm_add_save')}</button><button class="btn" type="button" onclick={() => (form = '')}>{t('i2_cancel')}</button></div>
	</form>
{:else if form === 'invite'}
	<form class="card p2form" style="margin-bottom:12px" onsubmit={sendInvite} aria-labelledby="fm-inv-h">
		<h3 id="fm-inv-h">{t('fm_invite')}<span class="r">{t('fm_inv_d')}</span></h3>
		<div class="p2row">
			<label style="flex:1;min-width:180px">{t('fm_name')}<input class="p2in" bind:value={inv.display_name} required maxlength="80" /></label>
			<label>{t('fm_lang')}
				<select class="p2in" bind:value={inv.locale}><option value="es">Español</option><option value="en">English</option></select>
			</label>
			<label>{t('fm_claim')}
				<select class="p2in" bind:value={inv.claim_profile}>
					{#if isOwner}<option value="">{t('fm_claim_none')}</option>{/if}
					{#each unclaimed as p (p.slug)}<option value={p.slug}>{p.display_name}</option>{/each}
				</select>
			</label>
		</div>
		<div class="note">{t('fm_claim_d')}</div>
		<div class="p2row"><button class="btn pri" type="submit" disabled={busy}>{t('fm_inv_make')}</button><button class="btn" type="button" onclick={() => (form = '')}>{t('i2_cancel')}</button></div>
	</form>
{/if}

{#if invited}
	<section class="card p2form" style="margin-bottom:12px" aria-live="polite">
		<h3>{tf('fm_inv_done', { name: invited.name })}<span class="r">{tf('fm_inv_exp', { n: invited.days })}</span></h3>
		<div class="p2secret" data-testid="invite-url">{invited.url}</div>
		<div class="muted" style="font-size:12px">{t('fm_inv_code')} <span class="mono" data-testid="invite-code">{invited.code}</span></div>
		<div class="note" style="color:var(--watch)">{t('fm_inv_once')}</div>
		<div class="p2row"><button class="btn" onclick={() => invited && copy(invited.url)}>{copied ? t('fm_copied') : t('fm_copy')}</button><button class="btn" onclick={() => (invited = null)}>{t('fm_inv_close')}</button></div>
	</section>
{/if}

<div class="mem">
	{#each profiles as p (p.id)}
		{@const on = p.slug === onScreen?.slug}
		<button
			class="mcard"
			style="text-align:left;{on ? 'border-color:var(--accent);background:var(--raised)' : ''}"
			aria-pressed={on}
			title={tf('h1_show', { name: p.display_name })}
			onclick={() => setProfile(p.slug)}
		>
			<div class="av" style="background:{color(p.slug)};width:34px;height:34px">{p.initials}</div>
			<div style="min-width:0">
				<div style="font-weight:600">{p.display_name}{#if on}<span class="sr-only"> · {t('h1_on_screen')}</span>{/if}</div>
				<div class="muted" style="font-size:11.5px">{rel(p)}{p.age != null ? ', ' + p.age : ''} · {tf('h1_your_role', { r: t('r_' + p.role).toLowerCase() })}</div>
				{#if p.is_minor}<div class="dim" style="font-size:11px">{t('h1_minor')}</div>{/if}
			</div>
		</button>
	{/each}
</div>

<div class="fam">
	<div class="grid" style="align-content:start">
		<div class="card">
			<h3>{t('h1_pedigree')}<span class="r">{t('h1_pedigree_r')}</span></h3>
			<div class="empty"><PhaseChip phase="v0.3" /> {t('h1_pedigree_d')}</div>
		</div>
	</div>
	<div class="grid" style="align-content:start">
		<div class="card">
			<h3>{t('h1_grants')}<span class="r">{t('h1_grants_r')}</span></h3>
			{#if hh.loading && !hh.data}
				<div class="empty">{t('loading')}</div>
			{:else}
				<table class="t gm">
					<thead>
						<tr><th>{t('h1_viewer_profile')}</th>{#each cols as c (c.slug)}<th>{first(c.display_name)}</th>{/each}</tr>
					</thead>
					<tbody>
						{#if hh.data}
							{#each hh.data.members as m (m.user_id)}
								<tr>
									<td>{first(m.display_name)}</td>
									{#each cols as c (c.slug)}
										{@const r = roleCell(hh.data.grants, m, c.slug, manages(c.slug))}
										<td>{#if r === '?'}<span class="dim" title={t('h1_grant_unknown')}>?</span>{:else if r}{t('r_' + r).toLowerCase()}{:else}<span class="dim">—</span>{/if}</td>
									{/each}
								</tr>
							{/each}
						{:else if me}
							<tr>
								<td>{first(me.user.display_name)} <span class="dim">({t('h1_you')})</span></td>
								{#each cols as c (c.slug)}<td>{t('r_' + c.role).toLowerCase()}</td>{/each}
							</tr>
						{/if}
					</tbody>
				</table>
				{#if hh.error}
					<div class="note" style="margin-top:6px">{t('h1_grants_me')}</div>
					<NotReady error={hh.error} onretry={load} />
				{/if}
			{/if}
			<div class="note" style="margin-top:6px">{t('h1_grants_note')}</div>
		</div>
		<div class="card">
			<h3>{t('h1_bg')}<span class="r">{t('h1_bg_r')}</span></h3>
			<div class="empty"><PhaseChip phase="v1.0" /> {t('h1_bg_d')}</div>
		</div>
	</div>
</div>

<StepUpDialog />
