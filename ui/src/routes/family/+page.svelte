<script lang="ts">
	// Family (mockup screen #family): the household's profiles, the profile switcher and the grant matrix.
	// Profiles come from GET /me; the full matrix from GET /household (lane G). Pedigree and hints are
	// v0.3, break-glass v1.0: their cards say so.
	import { getHousehold } from '#lib/api';
	import type { Household, ProfileSummary } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import { t } from '#lib/i18n/index.svelte';
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
	const roleIn = (userId: string, slug: string) => hh.data?.grants.find((g) => g.user_id === userId && g.profile === slug)?.role ?? null;
</script>

<div class="ph">
	<div>
		<h1>{t('title_fam')}</h1>
		{#if me}<div class="sub">{tf('h1_fam_sub', { hh: me.household.name, n: cols.length })}</div>{/if}
	</div>
	<div class="right"><PhaseChip phase={t('h1_ph_roles')} /><PhaseChip phase={t('h1_ph_ped')} /><PhaseChip phase={t('h1_ph_bg')} /></div>
</div>

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
										{@const r = roleIn(m.user_id, c.slug)}
										<td>{#if r}{t('r_' + r).toLowerCase()}{:else}<span class="dim">—</span>{/if}</td>
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
