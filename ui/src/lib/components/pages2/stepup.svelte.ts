// Step-up: some changes need a fresh login (tokens, consent, grants, unlocking). withStepUp runs the
// call; on 403 step_up_required it confirms it is you (passkey, or the password in StepUpDialog), then
// retries once.
import { ApiError } from '#lib/api/core';
import { stepUpBegin, stepUpFinish } from '#lib/api/manage';
import { usePasskey } from './webauthn';

export const stepUp = $state<{ asking: boolean; error: string | null; answer: ((pw: string | null) => void) | null }>({
	asking: false,
	error: null,
	answer: null
});

function askPassword(): Promise<string | null> {
	return new Promise((resolve) => {
		stepUp.asking = true;
		stepUp.answer = (pw) => {
			stepUp.asking = false;
			stepUp.answer = null;
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
		const pw = await askPassword();
		if (pw === null) return false;
		await stepUpFinish({ password: pw });
		return true;
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
