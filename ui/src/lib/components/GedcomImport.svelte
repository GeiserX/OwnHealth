<script lang="ts">
	// The family tree import of the Family screen (slice 24): a GEDCOM file goes to the server, which reads
	// its structure in memory and stores nothing of it. The person picks themselves by position; the names
	// on the picker are read here from the person's own file and never sent. Each relative then waits in
	// the review queue as a proposal. Data: POST /profiles/{p}/family/gedcom/preview and /family/gedcom.
	import { importGedcom, previewGedcom, type GedcomImported, type GedcomPreview, type GedcomRefusal } from '#lib/api/family';
	import type { ProfileSummary } from '#lib/api/types';
	import { errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { decodeTree, namesByPosition, pickable } from '#lib/logic/gedcom';

	let { profile }: { profile: ProfileSummary } = $props();

	let fileEl: HTMLInputElement | undefined = $state();
	let file = $state<File | null>(null);
	let names = $state<(string | null)[]>([]);
	let preview = $state<GedcomPreview | null>(null);
	let refusal = $state<GedcomRefusal | null>(null);
	let done = $state<GedcomImported | null>(null);
	let error = $state<{ text: string; raw: string } | null>(null);
	let busy = $state(false);
	let query = $state('');
	let me = $state<number | null>(null);

	// sending a tree needs the import action and read:records on this profile (routes/family_history.rs)
	const allowed = $derived(['owner', 'guardian', 'caregiver'].includes(profile.role) && profile.scopes.includes('read:records'));
	const shown = $derived(preview ? pickable(preview.people, names, query) : []);
	const hasNames = $derived(!!preview && names.length === preview.people.length);
	const sexWord = (s: string) => t(s === 'female' ? 'i2_sex_f' : s === 'male' ? 'i2_sex_m' : 'i2_sex_u');
	const refusalText = (r: GedcomRefusal) => tf('gd_bad_' + (t('gd_bad_' + r.reason) === 'gd_bad_' + r.reason ? 'other' : r.reason), { n: r.line ?? '' });

	function reset() {
		preview = null;
		refusal = null;
		done = null;
		error = null;
		me = null;
		query = '';
		names = [];
	}

	async function picked(e: Event) {
		const f = (e.currentTarget as HTMLInputElement).files?.[0] ?? null;
		(e.currentTarget as HTMLInputElement).value = '';
		if (!f) return;
		reset();
		file = f;
		busy = true;
		try {
			names = namesByPosition(decodeTree(new Uint8Array(await f.arrayBuffer())));
			const r = await previewGedcom(profile.slug, f);
			if ('refused' in r) refusal = r;
			else preview = r;
		} catch (err) {
			error = { text: errText(err), raw: errRaw(err) };
		} finally {
			busy = false;
		}
	}

	async function propose() {
		if (!file || me == null) return;
		busy = true;
		error = null;
		try {
			const r = await importGedcom(profile.slug, file, me);
			if ('refused' in r) refusal = r;
			else {
				done = r;
				preview = null;
				file = null;
				names = [];
			}
		} catch (err) {
			error = { text: errText(err), raw: errRaw(err) };
		} finally {
			busy = false;
		}
	}

	function personLine(p: GedcomPreview['people'][number]): string {
		const parts = [sexWord(p.sex)];
		if (p.born_year != null) parts.push(tf('gd_born', { y: p.born_year }));
		if (p.death_age != null) parts.push(tf('gd_died_at', { n: p.death_age }));
		else if (p.deceased) parts.push(t('gd_died'));
		parts.push(tf('gd_links', { p: p.parents.length, s: p.partners.length, c: p.children.length }));
		return parts.join(' · ');
	}
</script>

{#if allowed}
	<section class="card gd" aria-labelledby="gd-h">
		<h3 id="gd-h">{t('gd_title')}<span class="r">{t('gd_r')}</span></h3>
		<div class="note"><span>{t('gd_note')}</span></div>
		<div class="p2row" style="margin-top:8px">
			<button class="btn" onclick={() => fileEl?.click()} disabled={busy}>{t('gd_file')}</button>
			<input bind:this={fileEl} type="file" hidden accept=".ged,.gedcom,text/plain" onchange={picked} aria-label={t('gd_file')} />
			{#if preview}<span class="dim" style="font-size:12px">{tp('gd_people', preview.people.length)}</span>{/if}
		</div>
		{#if busy}<div class="muted" role="status" style="margin-top:8px">{t('gd_busy')}</div>{/if}
		{#if error}<div class="p2err" role="alert" style="margin-top:8px" title={error.raw || undefined}>{error.text}</div>{/if}
		{#if refusal}<div class="p2err" role="alert" style="margin-top:8px">{refusalText(refusal)}</div>{/if}

		{#if preview}
			<fieldset class="pick" disabled={busy}>
				<legend>{tf('gd_who', { name: profile.display_name })}</legend>
				<div class="dim" style="font-size:11.5px">{hasNames ? t('gd_names_local') : t('gd_no_names')}</div>
				<input class="p2in" type="search" bind:value={query} placeholder={hasNames ? t('gd_search') : t('gd_search_year')} aria-label={hasNames ? t('gd_search') : t('gd_search_year')} />
				<ul>
					{#each shown as p (p.i)}
						<li>
							<label>
								<input type="radio" name="gd-me" value={p.i} bind:group={me} />
								<span class="who">{(hasNames ? names[p.i] : null) ?? tf('gd_person', { n: p.i + 1 })}</span>
								<span class="dim">{personLine(p)}</span>
							</label>
						</li>
					{:else}
						<li class="dim">{t('gd_none')}</li>
					{/each}
				</ul>
				{#if preview.people.length > shown.length}<div class="dim" style="font-size:11.5px">{tf('gd_more', { n: preview.people.length - shown.length })}</div>{/if}
				<div class="p2row" style="margin-top:8px">
					<button class="btn pri" onclick={propose} disabled={busy || me == null}>{t('gd_propose')}</button>
					<button class="btn" onclick={() => { reset(); file = null; }} disabled={busy}>{t('gd_cancel')}</button>
				</div>
			</fieldset>
		{/if}

		{#if done}
			<div class="done" role="status">
				<div>{tp('gd_done', done.proposed.length)}</div>
				{#if done.already || done.outside || done.left_out}
					<div class="dim" style="font-size:11.5px">{tf('gd_done_d', { already: done.already, outside: done.outside, left: done.left_out })}</div>
				{/if}
				{#if done.proposed.length}<a href="/import-review">{t('gd_review')}</a>{/if}
			</div>
		{/if}
	</section>
{/if}

<style>
	.gd {
		margin-top: 12px;
	}
	.pick {
		border: 1px solid var(--line2);
		border-radius: 8px;
		margin: 10px 0 0;
		padding: 8px 10px;
		min-width: 0;
	}
	.pick legend {
		font-weight: 600;
		padding: 0 4px;
	}
	.pick input[type='search'] {
		width: 100%;
		box-sizing: border-box;
		margin: 6px 0;
	}
	.pick ul {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 320px;
		overflow-y: auto;
	}
	.pick li label {
		display: flex;
		gap: 8px;
		align-items: baseline;
		padding: 4px 2px;
		font-size: 12.5px;
		cursor: pointer;
	}
	.pick .who {
		font-weight: 600;
		flex: none;
	}
	.done {
		margin-top: 10px;
		display: grid;
		gap: 4px;
	}
</style>
