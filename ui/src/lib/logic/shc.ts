// SMART Health Card receive (slice 22): which sentence says why a card was not imported, whose card
// it is, and what a card document holds.

/** Every reason the server gives (ownhealth_ingest_structured::shc::REASONS, in that order). */
export const SHC_REASONS = [
	'no_card',
	'photo_too_large',
	'numeric',
	'chunk_missing',
	'chunks_unread',
	'too_many',
	'not_jws',
	'header',
	'payload',
	'issuer',
	'private_issuer',
	'keys',
	'unknown_kid',
	'key',
	'bad_signature',
	'expired',
	'not_yet',
	'revoked',
	'revocation_list'
] as const;

/** Why the issuer's keys or revocation list could not be read (shc::DETAILS); `http` is `http_<status>`. */
export const SHC_DETAILS = ['dns', 'timeout', 'connect', 'too_large', 'not_json', 'not_jwks', 'not_crl', 'other_kid', 'http'] as const;

/** The dict key for a refusal reason; a reason this UI does not know gets the general sentence. */
export function refusalKey(reason: string): string {
	return (SHC_REASONS as readonly string[]).includes(reason) ? `shc_r_${reason}` : 'shc_r_other';
}

/** The dict key for a detail code, or null for a code this UI does not know. */
export function detailKey(code: string): string | null {
	if (/^http_\d{3}$/.test(code)) return 'shc_d_http';
	return (SHC_DETAILS as readonly string[]).includes(code) ? `shc_d_${code}` : null;
}

/**
 * The values a refusal sentence takes. A keys or revocation-list detail is a code, worded here, so no
 * text from the server or the network reaches the sentence; a code this UI does not know gets the
 * general wording.
 */
export function refusalVars(reason: string, detail: string | null, iss: string | null, t: (key: string) => string): Record<string, string> {
	const vars: Record<string, string> = { iss: iss ?? '', detail: detail ?? '' };
	if (reason === 'keys' || reason === 'revocation_list') {
		const key = detail ? detailKey(detail) : null;
		vars.detail = key ? t(key).replace('{status}', (detail ?? '').slice(5)) : t('shc_d_other');
	}
	if (reason === 'chunks_unread' || reason === 'chunk_missing') {
		const [a, b] = (detail ?? '').split('/');
		vars.found = a ?? '';
		vars.missing = a ?? '';
		vars.total = b ?? '';
	}
	return vars;
}

/** What the card says about its patient: untrusted text from the card. */
export type ShcSubject = { name: string | null; birth_date: string | null };

/**
 * The warning for a card that may be someone else's, or null when the card names this profile's
 * person. `identity` is the server's matched, hinted, missing or mismatch.
 */
export function identityWarning(identity: string, card: ShcSubject, profile: string): { key: string; vars: Record<string, string> } | null {
	if (identity === 'matched') return null;
	if (identity === 'mismatch' && card.name && card.birth_date) return { key: 'shc_w_other', vars: { name: card.name, birth: card.birth_date, profile } };
	if (identity === 'mismatch' && card.name) return { key: 'shc_w_other_name', vars: { name: card.name, profile } };
	return { key: 'shc_w_unsure', vars: { profile } };
}

export type ShcEntry = { type: string | null; date: string | null; text: string | null };
export type ShcDocCard = { iss: string; host: string; kid: string; issued: string; expires: string | null; entries: ShcEntry[] };

/** The cards a document holds (its `flags.shc`), or null for a document that is not a card. */
export function shcCards(flags: unknown): ShcDocCard[] | null {
	const list = (flags as { shc?: unknown } | null)?.shc;
	if (!Array.isArray(list)) return null;
	return list.map((c) => ({
		iss: String(c?.iss ?? ''),
		host: String(c?.host ?? ''),
		kid: String(c?.kid ?? ''),
		issued: String(c?.issued ?? ''),
		expires: c?.expires ? String(c.expires) : null,
		entries: Array.isArray(c?.entries)
			? c.entries.map((e: Record<string, unknown>) => ({
					type: e?.type ? String(e.type) : null,
					date: e?.date ? String(e.date) : null,
					text: e?.text ? String(e.text) : null
				}))
			: []
	}));
}
