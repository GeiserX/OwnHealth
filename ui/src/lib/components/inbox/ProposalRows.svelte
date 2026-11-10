<script lang="ts">
	// An agent's observations proposal, row by row (agent-extraction.md 4): each value with its page, its
	// grounding flag and the server's other checks. Rows found on the page start ticked; a row not found
	// there waits until the person has compared it with the page and ticks it. Accept writes only the
	// ticked rows, and never runs with none ticked.
	import { tf } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { proposalAcceptEdits, proposalDefaultKeep, type ProposalRow } from '#lib/logic/inbox';

	let {
		rows,
		busy = false,
		documentId = null,
		onaccept,
		onreject
	}: {
		rows: ProposalRow[];
		busy?: boolean;
		documentId?: string | null;
		onaccept: (edits: { rows: number[] }) => void;
		onreject: () => void;
	} = $props();

	let kept = $state<number[]>([]);
	$effect(() => {
		kept = proposalDefaultKeep(rows);
	});
	const edits = $derived(proposalAcceptEdits(rows, kept));
	const toggle = (i: number, on: boolean) => (kept = on ? [...kept, i] : kept.filter((k) => k !== i));
	const reason = (r: string | null, d: string | null) => {
		const k = 'i2_reason_' + r;
		return t(k) !== k ? t(k) : (d ?? t('i2_reason_other'));
	};
</script>

<div class="pr">
	<div class="dim" style="font-size:11px">{t('fu_pr_d')}</div>
	<ul>
		{#each rows as r (r.index)}
			<li class:bad={r.grounding === 'ungrounded'} class:low={r.grounding === 'no_word_layer'}>
				<label>
					<input type="checkbox" checked={kept.includes(r.index)} disabled={!r.usable || busy} onchange={(e) => toggle(r.index, e.currentTarget.checked)} />
					<span class="nm">{r.name}</span>
					<span class="num">{r.value} {r.unit}</span>
					<span class="dim">p{r.page}</span>
				</label>
				{#if documentId && r.grounding === 'grounded'}<a class="dim" href="?doc={documentId}&page={r.page}">{t('fu_pr_on_page')}</a>{/if}
				{#if !r.usable}
					<div class="why" style="color:var(--out)">✕ {t('fu_pr_unusable')}</div>
				{:else if r.grounding === 'ungrounded'}
					<div class="why" style="color:var(--out)" role="note">✕ {t('fu_ungrounded')}</div>
				{:else if r.grounding === 'no_word_layer'}
					<div class="why" style="color:var(--watch)" role="note">● {t('fu_no_word_layer')}</div>
				{:else}
					<div class="why dim">{tf('fu_grounded', { p: r.page })}</div>
				{/if}
				{#each r.problems as c, i (i)}<div class="why" style="color:var(--watch)" title={c.detail ?? undefined}>● {reason(c.reason, c.detail)}</div>{/each}
			</li>
		{/each}
	</ul>
	<div class="acts">
		<button class="btn sm ok" disabled={busy || !edits} onclick={() => edits && onaccept(edits)}>{tf('fu_pr_accept', { n: edits?.rows.length ?? 0 })}</button>
		<button class="btn sm no" disabled={busy} onclick={onreject}>{t('i2_reject')}</button>
		{#if !edits}<span class="dim" style="font-size:11px">{t('fu_pr_none')}</span>{/if}
	</div>
</div>

<style>
	.pr {
		display: grid;
		gap: 4px;
		margin-top: 4px;
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	li {
		border-left: 3px solid var(--ok);
		padding: 2px 0 2px 8px;
		font-size: 12px;
	}
	li.bad {
		border-left-color: var(--out);
	}
	li.low {
		border-left-color: var(--watch);
	}
	label {
		display: inline-flex;
		gap: 8px;
		align-items: center;
	}
	.nm {
		font-weight: 560;
	}
	.why {
		font-size: 11px;
	}
	.acts {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
</style>
