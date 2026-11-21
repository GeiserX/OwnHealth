// SMART Health Card receive (slice 22): which sentence says why a card was not imported.

/** Every reason the server gives (ownhealth_ingest_structured::shc::Refusal::code). */
export const SHC_REASONS = [
	'no_card',
	'numeric',
	'chunk_missing',
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
	'revoked'
] as const;

/** The dict key for a refusal reason; a reason this UI does not know gets the general sentence. */
export function refusalKey(reason: string): string {
	return (SHC_REASONS as readonly string[]).includes(reason) ? `shc_r_${reason}` : 'shc_r_other';
}
