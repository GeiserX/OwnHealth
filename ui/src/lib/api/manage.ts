// Endpoints the "manage" pages read (lane I owns this file): wearables, import & review, family, MCP & access.
// Backends: lane C (wearables.py, records.py), lane B (documents.py), lane G (tokens.py, audit.py, family.py),
// lane F (mcp_usage.py). Review and uploads are in core.ts.
import { del, get, p, post, put } from './core';
import type { AuditOut, DocumentDetail, DocumentSummary, Household, RecordOut, Scope, TokenOut, TokenUsage, Uuid, WearablesSummary } from './types';

export const getWearables = (profile: string, lang?: string) => get<WearablesSummary>(`${p(profile)}/wearables/summary`, { lang });

export const listDocuments = (profile: string, q: { q?: string; type?: string } = {}) =>
	get<{ items: DocumentSummary[] }>(`${p(profile)}/documents`, q);
export const getDocument = (id: Uuid) => get<DocumentDetail>(`/documents/${id}`);
export const pageImageUrl = (id: Uuid, page: number) => `/api/documents/${id}/pages/${page}.png`;
export const documentFileUrl = (id: Uuid) => `/api/documents/${id}/file`;

export const listRecords = (profile: string, kind: RecordOut['kind']) => get<{ items: RecordOut[] }>(`${p(profile)}/records/${kind}`);

export const listTokens = () => get<{ items: TokenOut[] }>('/tokens');
export const createToken = (body: {
	label: string;
	scopes: Scope[];
	profiles: string[];
	default_profile?: string;
	days?: number;
	identity_mode?: TokenOut['identity_mode'];
	lang?: 'es' | 'en';
	units?: TokenOut['units'];
}) => post<{ token: TokenOut; plaintext: string }>('/tokens', body);
export const revokeToken = (id: Uuid) => del<{ id: Uuid; revoked: true }>(`/tokens/${id}`);
export const getTokenUsage = (id: Uuid) => get<TokenUsage>(`/tokens/${id}/usage`);

export const listAudit = (q: { profile?: string; limit?: number; before?: number } = {}) => get<{ items: AuditOut[] }>('/audit', q);

export const getHousehold = () => get<Household>('/household');
export const setGrant = (body: { user_id: Uuid; profile: string; role: string; categories?: Scope[] }) => put<{ ok: true }>('/grants', body);
export const setLocks = (profile: string, locks: Scope[]) => put<{ ok: true }>(`${p(profile)}/locks`, { locks });
