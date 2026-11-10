// Typed fetch wrapper and the foundation endpoints. Same-origin; the session cookie rides along.
import { filenameFromDisposition } from '#lib/logic/download';
import { validationDetail } from '#lib/logic/errors';
import { app } from '#lib/state/app.svelte';
import type { ImportItem, JobInfo, Me, ProfileSummary, ReviewItem, SourceDetail, Uuid } from './types';

export class ApiError extends Error {
	constructor(
		public status: number,
		public code: string,
		message: string,
		public allowed: string[] = [],
		// What the server says to do instead ('Ask by position instead: …'), when it says one.
		public fix: string | null = null
	) {
		super(message);
	}
}

type Query = Record<string, string | number | boolean | null | undefined>;

function withQuery(path: string, query?: Query): string {
	if (!query) return path;
	const qs = new URLSearchParams();
	for (const [k, v] of Object.entries(query)) if (v !== undefined && v !== null && v !== '') qs.set(k, String(v));
	const s = qs.toString();
	return s ? `${path}?${s}` : path;
}

async function send(method: string, path: string, opts: { query?: Query; body?: unknown; accept?: string } = {}): Promise<Response> {
	const init: RequestInit = { method, credentials: 'same-origin', headers: { Accept: opts.accept ?? 'application/json' } };
	if (opts.body instanceof FormData) init.body = opts.body;
	else if (opts.body !== undefined) {
		init.body = JSON.stringify(opts.body);
		(init.headers as Record<string, string>)['Content-Type'] = 'application/json';
	}
	const res = await fetch(withQuery(`/api${path}`, opts.query), init);
	if (!res.ok) {
		let code = 'http_' + res.status;
		let message = res.statusText;
		let allowed: string[] = [];
		let fix: string | null = null;
		try {
			const body = await res.json();
			const d = body?.detail;
			if (Array.isArray(d)) ({ code, message } = validationDetail(d));
			else if (d && typeof d === 'object') {
				code = d.code ?? code;
				message = d.message ?? message;
				allowed = d.allowed ?? [];
				fix = typeof d.fix === 'string' ? d.fix : null;
			} else if (typeof d === 'string') message = d;
		} catch {
			/* not JSON */
		}
		// A session that ended mid-use: the layout shows the sign-in screen instead of a broken page.
		if (res.status === 401 && !path.startsWith('/auth/')) app.unauthenticated = true;
		throw new ApiError(res.status, code, message, allowed, fix);
	}
	return res;
}

export async function request<T>(method: string, path: string, opts: { query?: Query; body?: unknown } = {}): Promise<T> {
	const res = await send(method, path, opts);
	return (res.status === 204 ? undefined : await res.json()) as T;
}

/** A file download (the export zip). Errors map like request(), so step_up_required still reaches withStepUp. */
export async function requestBlob(method: string, path: string, body: unknown, fallbackName: string): Promise<{ blob: Blob; filename: string }> {
	const res = await send(method, path, { body, accept: 'application/zip, application/octet-stream' });
	return { blob: await res.blob(), filename: filenameFromDisposition(res.headers.get('content-disposition'), fallbackName) };
}

export const get = <T>(path: string, query?: Query) => request<T>('GET', path, { query });
export const post = <T>(path: string, body?: unknown) => request<T>('POST', path, { body });
export const put = <T>(path: string, body?: unknown) => request<T>('PUT', path, { body });
export const del = <T>(path: string) => request<T>('DELETE', path);

/** Path segment for a profile slug. */
export const p = (slug: string) => `/profiles/${encodeURIComponent(slug)}`;

// ---- foundation endpoints (backend: ownhealth/api/core.py, review.py, imports.py) ----

export const getMe = () => get<Me>('/me');
export const getProfiles = () => get<ProfileSummary[]>('/profiles');
export const getSource = (srcId: string, includeText = false) =>
	get<SourceDetail>(`/sources/${encodeURIComponent(srcId)}`, { include_text: includeText || undefined });

export const listReview = (q: { status?: string; profile?: string; kind?: string; document_id?: Uuid } = {}) =>
	get<{ items: ReviewItem[]; open: number; withheld?: number }>('/review', q);
export const getReviewItem = (id: Uuid) => get<ReviewItem>(`/review/${id}`);
export const acceptReview = (id: Uuid, edits?: Record<string, unknown>) =>
	post<{ id: Uuid; status: 'accepted'; result_ids: Uuid[] }>(`/review/${id}/accept`, { edits: edits ?? null });
export const rejectReview = (id: Uuid, reason: string) => post<{ id: Uuid; status: 'rejected' }>(`/review/${id}/reject`, { reason });
export const reopenReview = (id: Uuid) => post<{ id: Uuid; status: 'open' }>(`/review/${id}/reopen`);
export const dismissReview = (id: Uuid, reason?: string) =>
	post<{ id: Uuid; status: 'dismissed' }>(`/review/${id}/dismiss`, reason ? { reason } : undefined);
export const bulkAccept = (documentId: Uuid, minRowConf: number) =>
	post<{ accepted: Uuid[] }>('/review/bulk-accept', { document_id: documentId, min_row_conf: minRowConf });

export function uploadFiles(profile: string, files: File[]): Promise<{ items: ImportItem[] }> {
	const form = new FormData();
	form.set('profile', profile);
	for (const f of files) form.append('files', f, f.name);
	return request('POST', '/imports', { body: form });
}
export const listJobs = (ids?: number[]) => get<{ jobs: JobInfo[] }>('/jobs', { ids: ids?.join(',') });
