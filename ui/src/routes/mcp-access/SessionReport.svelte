<script lang="ts">
	import { ApiError, getSessionReport, setSessionReportConsent, type SessionReportOut } from '#lib/api';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader } from '#lib/components/pages1/util.svelte';
	import { int } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { intlLocale } from '#lib/i18n/langs';

	const report = new Loader<SessionReportOut>();
	const load = () => report.run(getSessionReport);
	let saving = $state(false);
	let saveError = $state(false);
	const hidden = $derived(report.error instanceof ApiError && report.error.status === 403);
	const time = $derived(new Intl.DateTimeFormat(intlLocale(i18n.lang), { dateStyle: 'medium', timeStyle: 'medium' }));

	$effect(() => {
		load();
	});

	// A session with any shortfall opens, so a collapsed one never hides a failure.
	const fellShort = (s: SessionReportOut['sessions'][number]) =>
		s.tools.some((r) => r.not_assessed_calls + r.budget_cuts + r.denied + r.errors > 0);

	async function toggle(input: HTMLInputElement) {
		if (!report.data) return;
		// The box stays focusable while a save runs (aria-disabled), so a change then is undone here.
		if (saving) {
			input.checked = !input.checked;
			return;
		}
		const enabled = input.checked;
		saving = true;
		saveError = false;
		try {
			await withStepUp(() => setSessionReportConsent(enabled));
			// Clear old results as soon as the server confirms the new setting.
			report.data = { enabled, sessions: [] };
			if (enabled) await load();
		} catch {
			input.checked = report.data?.enabled ?? false;
			saveError = true;
		} finally {
			saving = false;
		}
	}
</script>

{#if !hidden && (report.data || report.error)}
	<section class="card" aria-labelledby="agent-session-report">
		<h3 id="agent-session-report">{t('i2_sr_title')}</h3>
		{#if report.error}
			<NotReady error={report.error} onretry={load} />
		{:else if report.data}
			<p class="note">{t('i2_sr_gap')}</p>
			<p class="note">{t('i2_sr_data')}</p>
			<label class="sr-consent">
				<input type="checkbox" checked={report.data.enabled} aria-disabled={saving} onchange={(event) => toggle(event.currentTarget)} />
				{t('i2_sr_consent')}
			</label>
			{#if saveError}<p class="p2err" role="alert">{t('i2_sr_save_error')}</p>{/if}
			{#if !report.data.enabled}
				<p class="note" role="status">{t('i2_sr_off')}</p>
			{:else if report.loading}
				<p class="note" role="status">{t('loading')}</p>
			{:else if report.data.sessions.length === 0}
				<p class="note" role="status">{t('i2_sr_empty')}</p>
			{:else}
				<p class="note">{t('i2_sr_recent')}</p>
				{#each report.data.sessions as session, i}
					<details open={i === 0 || fellShort(session)}>
						<summary>{time.formatRange(new Date(session.started_at), new Date(session.ended_at))}</summary>
						<div class="tscroll">
							<table class="t stack">
								<thead><tr>
									<th scope="col">{t('i2_sr_tool')}</th>
									<th scope="col">{t('i2_sr_calls')}</th>
									<th scope="col">{t('i2_sr_na_calls')}</th>
									<th scope="col">{t('i2_sr_na_items')}</th>
									<th scope="col">{t('i2_sr_cuts')}</th>
									<th scope="col">{t('i2_sr_denied')}</th>
									<th scope="col">{t('i2_sr_errors')}</th>
									<th scope="col">{t('i2_sr_bytes')}</th>
								</tr></thead>
								<tbody>
									{#each session.tools as row (row.tool)}
										<tr>
											<td data-label={t('i2_sr_tool')} class="mono">{row.tool === 'unknown' ? t('i2_sr_unknown') : row.tool}</td>
											<td data-label={t('i2_sr_calls')} class="num">{int(row.calls)}</td>
											<td data-label={t('i2_sr_na_calls')} class="num">{int(row.not_assessed_calls)}</td>
											<td data-label={t('i2_sr_na_items')} class="num">{int(row.not_assessed_n)}</td>
											<td data-label={t('i2_sr_cuts')} class="num">{int(row.budget_cuts)}</td>
											<td data-label={t('i2_sr_denied')} class="num">{int(row.denied)}</td>
											<td data-label={t('i2_sr_errors')} class="num">{int(row.errors)}</td>
											<td data-label={t('i2_sr_bytes')} class="num">{int(row.arg_bytes)}</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					</details>
				{/each}
			{/if}
		{/if}
	</section>
{/if}

<style>
	.sr-consent {
		display: flex;
		width: fit-content;
		align-items: center;
		gap: 8px;
		margin: 10px 0 6px;
	}
	.note { margin: 6px 0; }
	details { border-top: 1px solid var(--line); }
	summary { padding: 8px 0; cursor: pointer; }
</style>
