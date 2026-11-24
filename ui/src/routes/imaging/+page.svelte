<script lang="ts">
	// Imaging (mockup screen #imaging), v1.0 (slice 42). The studies are the timeline's imaging events
	// (GET /profiles/{p}/timeline?types=imaging, read:records): each names its study, the report the import
	// linked to it, and whether the viewer can open it. Import sends a CD folder or a zip to
	// POST /imaging/import and follows its import.dicom job; Open asks for a one-hour viewer link and opens
	// OHIF in a new tab, where it runs sandboxed (imaging_proxy.rs).
	import { onDestroy } from 'svelte';
	import { documentFileUrl, getTimeline, importImaging, listJobs } from '#lib/api';
	import type { JobInfo, Timeline, TimelineEvent } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { openViewer } from '#lib/imaging';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { imagingLines } from '#lib/logic/imaging';
	import { poller } from '#lib/logic/poll';
	import { currentProfile } from '#lib/state/app.svelte';

	type Err = { text: string; raw: string };
	const profile = $derived(currentProfile());
	const tl = new Loader<Timeline>();
	function load() {
		const slug = profile?.slug;
		const lang = i18n.lang;
		if (slug) tl.run(() => getTimeline(slug, { types: 'imaging', lang }), slug);
	}
	$effect(load);

	const studies = $derived(tl.data?.events ?? []);
	const withheld = $derived(tl.data?.withheld.by_scope.includes('imaging') ?? false);
	let picked = $state<string | null>(null);
	// the picked study, else the newest; another profile's list never keeps the old pick
	const study = $derived(studies.find((s) => s.study_id === picked) ?? studies[0] ?? null);

	// ---- open in the viewer ----
	let opening = $state(false);
	let openError = $state<Err | null>(null);
	async function open(s: TimelineEvent) {
		if (!profile || !s.study_instance_uid) return;
		opening = true;
		openError = null;
		try {
			await openViewer(profile.slug, s.study_instance_uid);
		} catch (e) {
			openError = { text: errText(e), raw: errRaw(e) };
		} finally {
			opening = false;
		}
	}
	$effect(() => {
		void study?.study_id;
		openError = null;
	});

	// ---- import a CD folder or a zip ----
	let folderInput = $state<HTMLInputElement>();
	let zipInput = $state<HTMLInputElement>();
	let sending = $state<number | null>(null); // files on their way up
	let upload = $state<{ slug: string; files: number; duplicate: boolean; jobId: number | null; job: JobInfo | null } | null>(null);
	let upError = $state<Err | null>(null);
	const follow = poller({
		tick: async () => {
			const id = upload?.jobId;
			if (id == null) return true;
			const job = (await listJobs([id])).jobs.find((j) => j.id === id) ?? null;
			if (upload) upload.job = job;
			return !job || job.state === 'done' || job.state === 'failed' || job.state === 'dead';
		},
		ondone: load,
		onfail: (e) => (upError = { text: errText(e), raw: errRaw(e) })
	});
	onDestroy(follow.stop);
	// an upload belongs to the profile it was sent for
	$effect(() => {
		if (upload && upload.slug !== profile?.slug) {
			follow.stop();
			upload = null;
			upError = null;
		}
	});

	async function send(input: HTMLInputElement | undefined) {
		const files = [...(input?.files ?? [])];
		const slug = profile?.slug;
		if (input) input.value = '';
		if (!slug || !files.length) return;
		follow.stop();
		upload = null;
		upError = null;
		sending = files.length;
		try {
			const r = await importImaging(slug, files);
			upload = { slug, files: r.files, duplicate: r.duplicate, jobId: r.job_id, job: null };
			follow.start();
		} catch (e) {
			upError = { text: errText(e), raw: errRaw(e) };
		} finally {
			sending = null;
		}
	}
	const job = $derived(upload?.job ?? null);
	const finished = $derived(job?.state === 'done' ? imagingLines(job.result) : null);
	const failed = $derived(job && (job.state === 'failed' || job.state === 'dead') ? job : null);
	const toReview = $derived(Number((job?.result as { in_review?: number } | null)?.in_review ?? 0) > 0);
</script>

<div class="ph">
	<div>
		<h1>{t('title_img')}</h1>
		<div class="sub">{tf('h1_img_sub', { name: profile?.display_name ?? '' })}{tl.data && !withheld ? ` · ${tp('img_count', studies.length)}` : ''}</div>
	</div>
	<div class="right">
		<input type="file" bind:this={folderInput} webkitdirectory multiple hidden onchange={() => send(folderInput)} />
		<input type="file" bind:this={zipInput} accept=".zip,application/zip" hidden onchange={() => send(zipInput)} />
		<button class="btn" disabled={sending != null || !profile} onclick={() => folderInput?.click()}>{t('h1_img_import')}</button>
		<button class="btn" disabled={sending != null || !profile} onclick={() => zipInput?.click()}>{t('img_import_zip')}</button>
	</div>
</div>

{#if sending != null || upload || upError}
	<div class="banner up" role="status">
		<div>
			{#if sending != null}{tp('img_uploading', sending)}
			{:else if upError}<span class="p2err" title={upError.raw}>{upError.text}</span>
			{:else if upload}
				{#if failed}<span class="p2err" title={failed.last_error ?? undefined}>{t('img_job_failed')} {failed.last_error ? errText(new Error(failed.last_error)) : ''}</span>
				{:else if finished}{finished.map((l) => tp(l.key, l.n)).join(' · ')}
				{:else}{upload.duplicate ? t('img_kept_dup') : tp('img_kept', upload.files)}{/if}
			{/if}
		</div>
		{#if toReview}<a class="btn sm" style="margin-left:auto" href="/import-review">{t('h1_img_go')}</a>{/if}
	</div>
{/if}

{#if tl.loading && !tl.data}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if tl.error}
	<div class="card"><NotReady error={tl.error} onretry={load} /></div>
{:else if withheld}
	<div class="card"><div class="empty">{tf('h1_withheld', { list: t('h1_ty_imaging') })}</div></div>
{:else if tl.data}
	<div class="img panes">
		<div class="card" style="padding:8px;overflow:auto">
			<h3 style="padding:2px 6px 0">{t('h1_img_studies')}</h3>
			{#each studies as s (s.study_id)}
				<button class="study" class:on={s.study_id === study?.study_id} aria-pressed={s.study_id === study?.study_id} onclick={() => (picked = s.study_id ?? null)}>
					<span class="row"><b>{s.title}</b>{#if s.detail}<span class="flag">{s.detail}</span>{/if}</span>
					<span class="dim">{[s.date, s.series != null ? tp('img_series_n', s.series) : '', s.instances != null ? tp('img_instances_n', s.instances) : ''].filter(Boolean).join(' · ')}</span>
					{#if s.src_label}<span class="dim">{s.src_label}</span>{/if}
				</button>
			{:else}
				<div class="empty">{t('img_none')}</div>
			{/each}
			<div class="note" style="margin-top:10px;padding:0 6px">{t('h1_img_note')}</div>
		</div>
		<div class="card">
			{#if study}
				<h3>{study.title}</h3>
				<table class="t" style="font-size:12px">
					<tbody>
						<tr><td class="dim">{t('img_date')}</td><td class="num">{study.date}</td></tr>
						<tr><td class="dim">{t('img_modality')}</td><td>{study.detail ?? '—'}</td></tr>
						<tr><td class="dim">StudyInstanceUID</td><td class="mono uid">{study.study_instance_uid}</td></tr>
						<tr><td class="dim">{t('img_series')}</td><td class="num">{study.series ?? '—'} / {study.instances ?? '—'}</td></tr>
						{#if study.src_label}<tr><td class="dim">{t('img_source')}</td><td>{study.src_label}</td></tr>{/if}
					</tbody>
				</table>
				<div style="margin-top:12px">
					{#if study.viewer_open}
						<button class="btn pri" disabled={opening} onclick={() => open(study)}>{t('h1_img_ohif')}</button>
						<div class="dim" style="font-size:11.5px;margin-top:6px">{t('img_viewer_d')}</div>
					{:else}
						<div class="dim" style="font-size:12px">{t('img_no_viewer')}</div>
					{/if}
					{#if openError}<div class="p2err" role="alert" style="margin-top:6px" title={openError.raw}>{openError.text}</div>{/if}
				</div>
			{:else}
				<div class="empty">{studies.length ? t('img_pick') : t('img_none')}</div>
			{/if}
		</div>
		<div class="card">
			<h3>{t('h1_img_report')}</h3>
			{#if study?.report_document_id}
				<a class="btn sm" href={documentFileUrl(study.report_document_id)} target="_blank" rel="noopener">{t('img_open_report')}</a>
				<div class="dim" style="font-size:12px;margin-top:6px;line-height:1.55">{t('img_report_d')}</div>
			{:else if study}
				<div class="dim" style="font-size:12px;line-height:1.55">{t('img_no_report')}</div>
			{/if}
		</div>
	</div>
{/if}

<style>
	.panes {
		height: auto;
		align-items: start;
	}
	.study {
		display: flex;
		flex-direction: column;
		gap: 2px;
		width: 100%;
		text-align: left;
		background: none;
		color: inherit;
		font: inherit;
	}
	.study .row {
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.study .row .flag {
		margin-left: auto;
	}
	.study .dim {
		font-size: 11.5px;
	}
	.uid {
		font-size: 10.5px;
		word-break: break-all;
	}
	.up {
		margin-bottom: 12px;
	}
</style>
