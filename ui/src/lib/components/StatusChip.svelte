<script lang="ts">
	// Status is never colour alone: the dot always comes with its word.
	import { t } from '#lib/i18n/index.svelte';
	import type { Status } from '#lib/api/types';

	let { status, label }: { status: Status | 'info'; label?: string } = $props();
	const KEY: Record<string, string> = { ok: 'ok', watch: 'watch', out: 'out', stale: 'stale', nodata: 'nodata', info: 'info' };
</script>

<span class="pill p-{status}"><i aria-hidden="true"></i>{label ?? t(KEY[status])}</span>
