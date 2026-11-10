// Step-up: some changes need a fresh login (tokens, consent, grants, unlocking). withStepUp runs the
// call; on 403 step_up_required it confirms it is you (passkey, or the password in StepUpDialog), then
// retries once. A wrong password keeps the dialog open with the reason; it closes only on success or Cancel.
import { ApiError } from '#lib/api/core';
import { stepUpBegin, stepUpFinish } from '#lib/api/manage';
import { errText } from '#lib/components/pages1/util.svelte';
import { askUntilRight } from '#lib/logic/people';
import { usePasskey } from './webauthn';

export const stepUp = $state<{
	asking: boolean;
	busy: boolean;
	error: string | null;
	// Grows on every wrong try, so the dialog clears and refocuses the field each time.
	tries: number;
	answer: ((pw: string | null) => void) | null;
}>({
	asking: false,
	busy: false,
	error: null,
	tries: 0,
	answer: null
});

function close() {
	stepUp.asking = false;
	stepUp.busy = false;
	stepUp.error = null;
	stepUp.answer = null;
}

/** Shows the dialog (or keeps it open with the last error) and resolves the typed password, or null on Cancel. */
function ask(error: string | null): Promise<string | null> {
	return new Promise((resolve) => {
		stepUp.busy = false;
		if (error !== null) {
			stepUp.error = error;
			stepUp.tries += 1;
		}
		stepUp.asking = true;
		stepUp.answer = (pw) => {
			stepUp.answer = null;
			if (pw === null) close();
			else stepUp.busy = true;
			resolve(pw);
		};
	});
}

async function confirmItIsYou(): Promise<boolean> {
	const how = await stepUpBegin();
	if (how.method === 'passkey' && how.options) {
		await stepUpFinish({ credential: await usePasskey(how.options) });
		return true;
	}
	if (how.method === 'password') {
		try {
			return await askUntilRight(
				ask,
				async (pw) => {
					await stepUpFinish({ password: pw });
				},
				(e) => (e instanceof ApiError && e.status === 401 ? errText(e) : null)
			);
		} finally {
			close();
		}
	}
	if (how.method === 'oidc' && how.url) location.href = how.url;
	return false;
}

export async function withStepUp<T>(fn: () => Promise<T>): Promise<T> {
	try {
		return await fn();
	} catch (e) {
		if (!(e instanceof ApiError && e.code === 'step_up_required')) throw e;
		if (!(await confirmItIsYou())) throw e;
		return fn();
	}
}
