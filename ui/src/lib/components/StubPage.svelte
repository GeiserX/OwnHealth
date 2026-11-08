<script lang="ts">
	// Placeholder body for a screen a UI lane has not filled yet. Delete when the page lands.
	import { t } from '#lib/i18n/index.svelte';
	import Card from './Card.svelte';
	import PageHeader from './PageHeader.svelte';
	import PhaseChip from './PhaseChip.svelte';

	let { titleKey, phase = 'v0.1', mockup }: { titleKey: string; phase?: string; mockup: string } = $props();
</script>

<PageHeader title={t(titleKey)}>
	{#snippet right()}<PhaseChip {phase} />{/snippet}
</PageHeader>
<Card>
	<div class="empty">{t('stub')} <span class="mono">docs/mockup/index.html#{mockup}</span></div>
</Card>
