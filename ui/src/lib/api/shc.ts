// SMART Health Card receive (slice 22; backend: ownhealth-api/src/routes/shc.rs).
// A refused card is an answer here, not an exception: the panel words each reason in es and en.
import { ApiError } from './core';
import type { Uuid } from './types';
import { app } from '#lib/state/app.svelte';

export type ShcIssuer = { iss: string; host: string };
export type ShcCard = { iss: string; issued: string; expires: string | null; types: string[] };
export type ShcSummary = { document_id: string; accepted: number; queued: number; skipped: number; reasons: Record<string, number> };
export type ShcAnswer =
	| { status: 'needs_approval'; issuers: ShcIssuer[] }
	| { status: 'imported'; document_id: Uuid; created: boolean; cards: ShcCard[]; summary: ShcSummary }
	| { status: 'refused'; reason: string; iss: string | null; detail: string | null; message: string };

/** What the person gave: pasted shc:/ text, or a .smart-health-card file or QR photo. */
export type ShcSource = { text: string } | { file: File };

export async function receiveCard(profile: string, source: ShcSource, approve: string[]): Promise<ShcAnswer> {
	const form = new FormData();
	form.set('profile', profile);
	if ('text' in source) form.set('text', source.text);
	else form.set('file', source.file, source.file.name);
	for (const host of approve) form.append('approve', host);
	const res = await fetch('/api/shc', { method: 'POST', credentials: 'same-origin', headers: { Accept: 'application/json' }, body: form });
	const body = await res.json().catch(() => null);
	const d = body?.detail;
	if (res.status === 422 && d?.code === 'shc_refused') {
		return { status: 'refused', reason: String(d.reason), iss: d.iss ?? null, detail: d.detail ?? null, message: String(d.message ?? '') };
	}
	if (!res.ok) {
		if (res.status === 401) app.unauthenticated = true;
		throw new ApiError(res.status, d?.code ?? 'http_' + res.status, d?.message ?? res.statusText, d?.allowed ?? []);
	}
	return body as ShcAnswer;
}
