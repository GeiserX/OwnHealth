<script lang="ts">
	// Asks for the password when a change needs a fresh login and this install has no passkeys (plain-HTTP LAN).
	import { t } from '#lib/i18n/index.svelte';
	import { stepUp } from './stepup.svelte';

	let dialog: HTMLDialogElement | undefined = $state();
	let pw = $state('');

	$effect(() => {
		if (!dialog) return;
		if (stepUp.asking && !dialog.open) {
			pw = '';
			dialog.showModal();
		} else if (!stepUp.asking && dialog.open) dialog.close();
	});
	const answer = (v: string | null) => stepUp.answer?.(v);
</script>

<dialog bind:this={dialog} class="p2dlg" aria-labelledby="su-h" onclose={() => stepUp.asking && answer(null)}>
	<form
		class="card"
		onsubmit={(e) => {
			e.preventDefault();
			answer(pw);
		}}
	>
		<h3 id="su-h">{t('i2_su_t')}</h3>
		<p class="muted" style="margin:0 0 8px">{t('i2_su_d')}</p>
		<label class="dim" for="su-pw" style="font-size:11px">{t('i2_pw')}</label>
		<input id="su-pw" class="p2in" type="password" autocomplete="current-password" bind:value={pw} required />
		<div style="display:flex;gap:6px;justify-content:flex-end;margin-top:10px">
			<button type="button" class="btn" onclick={() => answer(null)}>{t('i2_cancel')}</button>
			<button type="submit" class="btn pri">{t('i2_confirm')}</button>
		</div>
	</form>
</dialog>
