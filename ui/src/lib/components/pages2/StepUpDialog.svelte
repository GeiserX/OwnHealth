<script lang="ts">
	// Asks for the password when a change needs a fresh login and this install has no passkeys (plain-HTTP LAN).
	// A wrong password stays here: the reason shows inside the dialog and the field is cleared for another try.
	import { tick } from 'svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { stepUp } from './stepup.svelte';

	let dialog: HTMLDialogElement | undefined = $state();
	let input: HTMLInputElement | undefined = $state();
	let pw = $state('');

	$effect(() => {
		if (!dialog) return;
		if (stepUp.asking && !dialog.open) {
			pw = '';
			dialog.showModal();
		} else if (!stepUp.asking && dialog.open) dialog.close();
	});
	// Each wrong try: empty the field and put the cursor back in it.
	$effect(() => {
		if (!stepUp.tries) return;
		pw = '';
		tick().then(() => input?.focus());
	});
	const answer = (v: string | null) => stepUp.answer?.(v);
</script>

<dialog bind:this={dialog} class="p2dlg" aria-labelledby="su-h" oncancel={(e) => stepUp.busy && e.preventDefault()} onclose={() => stepUp.asking && answer(null)}>
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
		<input
			id="su-pw"
			class="p2in"
			style="width:100%"
			type="password"
			autocomplete="current-password"
			bind:this={input}
			bind:value={pw}
			required
			aria-invalid={stepUp.error ? 'true' : undefined}
			aria-describedby={stepUp.error ? 'su-err' : undefined}
		/>
		{#if stepUp.error}<div id="su-err" class="p2err" role="alert" style="margin-top:6px">{stepUp.error}</div>{/if}
		<div style="display:flex;gap:6px;justify-content:flex-end;margin-top:10px">
			<button type="button" class="btn" onclick={() => answer(null)}>{t('i2_cancel')}</button>
			<button type="submit" class="btn pri" disabled={stepUp.busy}>{t('i2_confirm')}</button>
		</div>
	</form>
</dialog>
