<script lang="ts">
	// A SMART Health Card document (slice 22): who signed each card, when, and the rows it holds. Its
	// values were imported from the signed card, so nothing here asks the person to type them in, and
	// the document has no page images to request.
	import { t } from '#lib/i18n/index.svelte';
	import { tf } from '#lib/components/pages1/util.svelte';
	import { day } from '#lib/components/pages2/fmt';
	import type { ShcDocCard } from '#lib/logic/shc';

	let { cards, queued, photo }: { cards: ShcDocCard[]; queued: number; photo: string | null } = $props();

	const typeWord = (type: string | null) => {
		if (!type) return '—';
		const key = 'shc_rt_' + type;
		return t(key) !== key ? t(key) : type;
	};
</script>

<div class="shcdoc" aria-label={t('shc_doc_t')}>
	{#each cards as c, n (n)}
		<section class="one">
			<div><span class="chip">{t('shc_signed_by')}</span> <span class="mono">{c.iss}</span></div>
			<div class="dim" style="font-size:11.5px;margin-top:2px">
				{tf('shc_issued', { date: day(c.issued) })}{#if c.expires} · {tf('shc_expires', { date: day(c.expires) })}{/if} · <span class="mono">{tf('shc_doc_kid', { kid: c.kid.slice(0, 12) + '…' })}</span>
			</div>
			<div style="font-weight:600;margin-top:8px;font-size:12.5px">{t('shc_doc_rows')}</div>
			{#if c.entries.length}
				<table class="t">
					<tbody>
						{#each c.entries as e, m (m)}
							<tr><td>{typeWord(e.type)}</td><td class="num dim" style="white-space:nowrap">{e.date ? day(e.date) : '—'}</td><td class="p2untrusted">{e.text ?? '—'}</td></tr>
						{/each}
					</tbody>
				</table>
			{:else}
				<div class="dim">{t('shc_doc_none')}</div>
			{/if}
		</section>
	{/each}
	{#if queued > 0}<div class="note" style="margin-top:8px"><span>{t('shc_doc_review')}</span></div>{/if}
	{#if photo}<img class="photo" src={photo} alt={t('shc_doc_t')} />{/if}
</div>

<style>
	.shcdoc {
		padding: 4px 2px;
		overflow-wrap: anywhere;
	}
	.one + .one {
		margin-top: 12px;
		padding-top: 12px;
		border-top: 1px solid var(--line2);
	}
	table {
		margin-top: 4px;
		width: 100%;
	}
	.photo {
		display: block;
		max-width: 100%;
		max-height: 320px;
		margin-top: 10px;
		border-radius: 6px;
		opacity: 0.9;
	}
</style>
