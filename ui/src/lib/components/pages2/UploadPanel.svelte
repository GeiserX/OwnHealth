<script lang="ts">
	// Upload into the current profile: lab PDF or photo, Apple Health export.zip, FHIR / IPS bundle,
	// or OwnGenome output (genome-facts.json plus its VCF and index, as files or the whole folder).
	// Each file becomes an import job; this panel polls the jobs until they finish.
	import { listJobs, uploadFiles } from '#lib/api';
	import type { ImportItem, JobInfo } from '#lib/api/types';
	import { t } from '#lib/i18n/index.svelte';
	import { errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { poller } from '#lib/logic/poll';

	// ondone gets the documents the finished jobs made, in upload order; rowsOf counts a document's rows to review.
	let {
		profile,
		profileName,
		ondone,
		rowsOf = () => 0
	}: { profile: string; profileName: string; ondone: (documentIds: string[]) => void; rowsOf?: (documentId: string) => number } = $props();

	let items = $state<ImportItem[]>([]);
	let jobs = $state<Record<number, JobInfo>>({});
	let error = $state<{ text: string; raw: string } | null>(null);
	let skipped = $state(0);
	let busy = $state(false);
	let over = $state(false);
	let fileEl: HTMLInputElement | undefined = $state();
	let dirEl: HTMLInputElement | undefined = $state();

	// OwnGenome folders hold reports too; only these travel.
	const GENOME_FILE = /(\.json|\.ndjson|\.vcf\.gz|\.vcf\.bgz|\.tbi)$/i;
	const TERMINAL = new Set(['done', 'failed', 'dead']);

	async function send(list: File[], fromFolder = false) {
		// A drop does not go through the disabled buttons, so a second upload is refused here.
		if (busy) return;
		const files = fromFolder ? list.filter((f) => GENOME_FILE.test(f.name)) : list;
		skipped = list.length - files.length;
		error = null;
		if (!files.length) {
			error = { text: t('i2_up_none'), raw: '' };
			return;
		}
		busy = true;
		try {
			const res = await uploadFiles(profile, files);
			items = res.items;
			poll();
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}

	// One polling loop at a time; it ends when the jobs finish, the panel unmounts, or listJobs keeps failing.
	const jobPoll = poller({
		tick: async () => {
			const ids = items.map((i) => i.job_id).filter((x): x is number => typeof x === 'number');
			if (!ids.length) return true;
			const res = await listJobs(ids);
			for (const j of res.jobs) jobs[j.id] = j;
			return ids.every((id) => jobs[id] && TERMINAL.has(jobs[id].state));
		},
		ondone: () => ondone(docIds()),
		onfail: (e) => (error = { text: errText(e), raw: errRaw(e) })
	});
	const poll = () => jobPoll.start();
	// The document each upload made, from the upload answer or its finished job.
	const docOf = (i: ImportItem) => i.document_id ?? (i.job_id != null ? (jobs[i.job_id]?.document_id ?? undefined) : undefined);
	const docIds = () => items.filter((i) => (i.job_id == null || jobs[i.job_id]?.state === 'done')).map(docOf).filter((x): x is string => !!x);
	$effect(() => jobPoll.stop);

	function stateOf(i: ImportItem): { word: string; cls: string } {
		if (i.duplicate) return { word: t('i2_up_dup'), cls: 'p-stale' };
		if (i.attached_to) return { word: t('i2_up_attached'), cls: 'p-stale' };
		const j = i.job_id != null ? jobs[i.job_id] : undefined;
		if (!j) return { word: t('i2_job_queued'), cls: 'p-info' };
		return { word: t('i2_job_' + j.state), cls: j.state === 'done' ? 'p-ok' : j.state === 'failed' || j.state === 'dead' ? 'p-out' : 'p-info' };
	}
	const files = (e: Event) => Array.from((e.currentTarget as HTMLInputElement).files ?? []);
</script>

<section
	class="card up"
	class:over
	aria-label={t('i2_upload')}
	ondragover={(e) => {
		e.preventDefault();
		over = true;
	}}
	ondragleave={() => (over = false)}
	ondrop={(e) => {
		e.preventDefault();
		over = false;
		send(Array.from(e.dataTransfer?.files ?? []));
	}}
>
	<h3>{tf('i2_up_t', { name: profileName })}<span class="r">{t('i2_up_kinds')}</span></h3>
	<div class="p2row">
		<button class="btn pri" onclick={() => fileEl?.click()} disabled={busy}>{t('i2_up_files')}</button>
		<button class="btn" onclick={() => dirEl?.click()} disabled={busy}>{t('i2_up_folder')}</button>
		<span class="muted" style="font-size:12px">{t('i2_up_drop')}</span>
		<input bind:this={fileEl} type="file" multiple hidden accept=".pdf,image/*,.heic,.zip,.json,.ndjson,.gz,.bgz,.tbi,.db,.sqlite,.sqlite3,application/vnd.sqlite3,application/x-sqlite3,application/octet-stream" onchange={(e) => send(files(e))} />
		<input bind:this={dirEl} type="file" hidden {...{ webkitdirectory: true }} onchange={(e) => send(files(e), true)} />
	</div>
	<div class="note" style="margin-top:8px">
		<span>{t('i2_up_genome')} <a href="https://github.com/GeiserX/OwnGenome" target="_blank" rel="noopener">OwnGenome</a> (https://github.com/GeiserX/OwnGenome).</span>
	</div>
	{#if busy}<div class="muted" role="status" style="margin-top:8px">{t('i2_up_sending')}</div>{/if}
	{#if error}<div class="p2err" role="alert" style="margin-top:8px" title={error.raw || undefined}>{error.text}</div>{/if}
	{#if skipped}<div class="dim" style="margin-top:6px;font-size:11.5px">{tf('i2_up_skipped', { n: skipped })}</div>{/if}
	{#if items.length}
		<div class="tscroll">
		<table class="t" style="margin-top:8px" aria-live="polite">
			<tbody>
				{#each items as i (i.sha256 + i.filename)}
					{@const s = stateOf(i)}
					{@const j = i.job_id != null ? jobs[i.job_id] : undefined}
					{@const d = docOf(i)}
					{@const n = d && j?.state === 'done' ? rowsOf(d) : 0}
					<tr>
						<td style="overflow-wrap:anywhere">{#if d && (j?.state === 'done' || i.duplicate)}<a href="?doc={d}">{i.filename}</a>{:else}{i.filename}{/if}</td>
						<td class="dim" title={i.kind}>{t('i2_ik_' + i.kind) !== 'i2_ik_' + i.kind ? t('i2_ik_' + i.kind) : i.kind}</td>
						<td style="white-space:nowrap"><span class="pill {s.cls}"><i aria-hidden="true"></i>{s.word}</span>{#if n}<a class="dim" style="font-size:11.5px;margin-left:8px" href="?doc={d}">{tp('i2_up_rows', n)}</a>{/if}</td>
						<td class="dim" style="font-size:11.5px" title={j?.last_error ?? undefined}>{j?.last_error ? errText(new Error(j.last_error)) : ''}</td>
					</tr>
				{/each}
			</tbody>
		</table>
		</div>
	{/if}
</section>

<style>
	.up {
		margin-bottom: 12px;
		border-style: dashed;
		border-color: var(--line2);
	}
	.up.over {
		border-color: var(--accent);
		background: var(--accent-bg);
	}
</style>
