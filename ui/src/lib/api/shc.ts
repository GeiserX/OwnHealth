// SMART Health Card receive (slice 22; backend: ownhealth-api/src/routes/shc.rs).
// A refused card is an answer here, not an exception: the panel words each reason in es and en.
import { ApiError } from './core';
import type { Uuid } from './types';
import { app } from '#lib/state/app.svelte';

/** One question: may OwnHealth fetch keys from this host, for these issuers? */
export type ShcHost = { host: string; issuers: string[] };
/** `name` and `birth_date` are what the card says, not checked against anything. */
export type ShcCard = { iss: string; host: string; issued: string; expires: string | null; types: string[]; name: string | null; birth_date: string | null };
export type ShcSummary = { document_id: string; accepted: number; queued: number; skipped: number; reasons: Record<string, number> };
/** A SMART Health Link as the person sees it before approving its host (slice 23). `label` is the sender's text. */
export type ShlLink = { host: string; label: string | null; expires: string | null; passcode: boolean; direct: boolean };
/** A file a link held, stored before it was decrypted; sent back with the yes for its card issuers. */
export type ShlHeld = { sha: string; content_type: string };
export type ShlNotImported = { content_type: string; reason: string };
export type ShcAnswer =
	| { status: 'needs_approval'; hosts: ShcHost[]; link?: ShlLink; held?: ShlHeld[] }
	| {
			status: 'imported';
			document_id: Uuid | null;
			created: boolean;
			identity: string;
			cards: ShcCard[];
			summary: ShcSummary;
			link?: ShlLink;
			documents?: { document_id: Uuid; created: boolean; content_type: string }[];
			not_imported?: ShlNotImported[];
	  }
	| { status: 'refused'; kind: 'card' | 'link'; reason: string; iss: string | null; detail: string | null; message: string };

/** What a link import adds to the form: the passcode, typed once, and the files held by an earlier answer. */
export type ShlExtra = { passcode?: string; held?: ShlHeld[] };

/** What the person gave: pasted shc:/ text, or a .smart-health-card file or QR photo. */
export type ShcSource = { text: string } | { file: File };

export async function receiveCard(profile: string, source: ShcSource, approve: string[], extra: ShlExtra = {}): Promise<ShcAnswer> {
	const form = new FormData();
	form.set('profile', profile);
	if ('text' in source) form.set('text', source.text);
	else form.set('file', source.file, source.file.name);
	for (const host of approve) form.append('approve', host);
	// the passcode goes in the body only, never in a URL
	if (extra.passcode) form.set('passcode', extra.passcode);
	for (const h of extra.held ?? []) form.append('held', `${h.sha} ${h.content_type}`);
	const res = await fetch('/api/shc', { method: 'POST', credentials: 'same-origin', headers: { Accept: 'application/json' }, body: form });
	const body = await res.json().catch(() => null);
	const d = body?.detail;
	if (res.status === 422 && (d?.code === 'shc_refused' || d?.code === 'shl_refused')) {
		const kind = d.code === 'shl_refused' ? 'link' : 'card';
		return { status: 'refused', kind, reason: String(d.reason), iss: d.iss ?? null, detail: d.detail ?? null, message: String(d.message ?? '') };
	}
	if (!res.ok) {
		if (res.status === 401) app.unauthenticated = true;
		throw new ApiError(res.status, d?.code ?? 'http_' + res.status, d?.message ?? res.statusText, d?.allowed ?? []);
	}
	return body as ShcAnswer;
}
