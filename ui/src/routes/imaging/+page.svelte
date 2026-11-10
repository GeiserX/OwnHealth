<script lang="ts">
	// Imaging (mockup screen #imaging). The imaging profile (DICOM import, Orthanc, OHIF) ships in v1.0, so
	// this page keeps the mockup's three-pane look and says plainly that nothing here works yet.
	import { tf } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';

	const profile = $derived(currentProfile());
	const TOOLS = ['W/L', 'Zoom', 'Pan', 'Length', 'Angle', 'Stack'];
</script>

<div class="ph">
	<div>
		<h1>{t('title_img')}</h1>
		<div class="sub">{tf('h1_img_sub', { name: profile?.display_name ?? '' })}</div>
	</div>
	<div class="right">
		<PhaseChip phase={t('h1_ph_img')} />
		<button class="btn" aria-disabled="true" title={t('h1_soon_btn')}>{t('h1_img_import')}</button>
	</div>
</div>

<div class="banner" style="border-color:rgba(110,168,254,.45);background:linear-gradient(90deg,rgba(110,168,254,.12),rgba(110,168,254,.03))" role="status">
	<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#6ea8fe" stroke-width="2" aria-hidden="true"><circle cx="12" cy="12" r="9" /><path d="M12 8v.5M12 11v5" /></svg>
	<div>
		<b>{t('h1_img_soon')}</b>
		<div class="muted" style="font-size:12px">{t('h1_img_d')}</div>
	</div>
	<a class="btn sm" style="margin-left:auto" href="/import-review">{t('h1_img_go')}</a>
</div>

<div class="img under-banner">
	<div class="card" style="padding:8px;overflow:auto">
		<h3 style="padding:2px 6px 0">{t('h1_img_studies')}</h3>
		<div class="empty">{t('h1_img_none')}</div>
		<div class="note" style="margin-top:10px;padding:0 6px">{t('h1_img_note')}</div>
	</div>
	<div class="viewer" aria-label={t('h1_img_viewer')}>
		<div class="vtool" aria-hidden="true">
			{#each TOOLS as tool (tool)}<span>{tool}</span>{/each}<span style="margin-left:auto">1×1</span><span>MPR</span>
		</div>
		<div style="flex:1;display:grid;place-items:center;min-height:0">
			<div style="text-align:center;color:#aeb5c2;font-size:12.5px">
				<svg width="44" height="44" viewBox="0 0 24 24" fill="none" stroke="#56607a" stroke-width="1.4" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="2" /><circle cx="12" cy="12" r="4" /></svg>
				<div style="margin-top:6px">{t('h1_img_viewer')}</div>
				<div style="margin-top:8px"><PhaseChip phase="v1.0" /></div>
			</div>
		</div>
		<div class="thumbs">
			<div style="margin-left:auto;border:0;width:auto;display:flex;align-items:center">
				<button class="btn pri" aria-disabled="true" title={t('h1_soon_btn')}>{t('h1_img_ohif')}</button>
			</div>
		</div>
	</div>
	<div class="card" style="overflow:auto">
		<h3>{t('h1_img_report')}</h3>
		<div class="dim" style="font-size:12px;line-height:1.55">{t('h1_img_report_d')}</div>
	</div>
</div>

<style>
	/* The panes fill the window below the banner; under 1100 px app.css lets them grow with their content. */
	.under-banner {
		height: calc(100vh - 52px - 40px - 52px - 100px);
	}
</style>
