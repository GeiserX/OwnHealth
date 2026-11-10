// Which words a person reads for an error. The server answers in English with a stable code; the UI shows
// its own sentence for the code or for a common cause, and falls back to the server's message.
// errText() in components/pages1/util.svelte.ts wraps this with t(); this file stays pure for the tests.

// Common causes, matched on the message after its 'XxxError: ' prefix. Order matters: first match wins.
const PATTERNS: [RegExp, string][] = [
	[/connection refused/i, 'err_refused'],
	[/error sending request|failed to connect|connection (timed out|reset)|operation timed out/i, 'err_unreachable'],
	[/failed to lookup address|nodename nor servname|name or service not known|dns error/i, 'err_dns'],
	[/\btls\b|certificate|handshake/i, 'err_tls'],
	[/wrong name or password/i, 'err_login'],
	[/^wrong password/i, 'err_stepup_pw'],
	[/unknown or used recovery code/i, 'err_recovery'],
	[/authenticat(e|ion) failed|login (was )?rejected|invalid credentials|\bLOGIN\b.*\bNO\b/i, 'err_auth_rejected'],
	[/already on the list for another profile/i, 'err_sender_other'],
	[/already on the list/i, 'err_sender_dup'],
	[/already reads this folder/i, 'err_folder_overlap'],
	[/not a PDF, image/i, 'err_unsupported_file'],
	[/^not a decimal|^the value is not a number/i, 'err_decimal'],
	[/^the date is in the future/i, 'err_future_date'],
	[/^the birth date is in the future/i, 'err_birth_future'],
	[/^the birth date is before 1900/i, 'err_birth_old'],
	[/^the country is two letters/i, 'err_country'],
	[/^failed to fetch$|networkerror|load failed/i, 'err_no_connection'],
	[/source is switched off/i, 'err_switched_off'],
	[/demo never reads the inbox folder/i, 'err_demo_folder']
];

// The browser's WebAuthn refusals (a DOMException name), which it words in its own language.
const BROWSER: Record<string, string> = {
	SecurityError: 'wa_security',
	NotAllowedError: 'wa_not_allowed',
	InvalidStateError: 'wa_invalid_state',
	AbortError: 'wa_abort'
};

/** Our code for a browser error name ('SecurityError' → 'wa_security'), or null. */
export function browserErrorCode(name: string): string | null {
	return BROWSER[name] ?? null;
}

/** The message without a leading exception name ('ImapError: connect: …' → 'connect: …'). */
export function stripErrorName(message: string): string {
	return message.replace(/^\s*\w*(Error|Exception):\s*/, '');
}

/** The prefix name of a message ('PasswordError'), or null. */
export function errorName(message: string): string | null {
	const m = /^\s*(\w*(?:Error|Exception)):/.exec(message);
	return m ? m[1] : null;
}

/**
 * The dict key that says this error in plain words, or null to show the server's own message.
 * `has` tells whether a key exists in the dict.
 */
export function errKey(code: string | null | undefined, message: string, has: (k: string) => boolean): string | null {
	if (code && has('err_' + code)) return 'err_' + code;
	if (errorName(message) === 'PasswordError') return 'err_pdf_password';
	const m = stripErrorName(message);
	for (const [re, key] of PATTERNS) if (re.test(m) && has(key)) return key;
	return null;
}

/**
 * The code and message of a validation answer, `{"detail": [{"loc": [...], "msg": "..."}]}`: the last
 * part of the place that failed, then what is wrong ('password: String should have at least 12 characters').
 */
export function validationDetail(detail: unknown[]): { code: string; message: string } {
	const last = detail[detail.length - 1] as { loc?: unknown[]; msg?: string } | undefined;
	const loc = Array.isArray(last?.loc) ? last.loc.filter((x) => x !== 'body') : [];
	const field = loc.length ? String(loc[loc.length - 1]) : '';
	const msg = last?.msg ?? 'invalid';
	return { code: 'invalid', message: field ? `${field}: ${msg}` : msg };
}
