// The share-link routes of one profile (backend: routes/shl.rs, CONTRACTS.md "v1.0 sharing"). The
// link itself, with its key, is only in the create answer; nothing here keeps or logs it.
import { del, get, p, post } from '#lib/api/core';
import type { ShlAccessList, ShlCreateIn, ShlCreated, ShlList, ShlRevoked, Uuid } from '#lib/api/types';

export const listLinks = (slug: string) => get<ShlList>(`${p(slug)}/shl`);
/** Needs step-up: call it through withStepUp. The PIN travels in this body only, never in a URL. */
export const createLink = (slug: string, body: ShlCreateIn) => post<ShlCreated>(`${p(slug)}/shl`, body);
export const linkAccess = (slug: string, id: Uuid) => get<ShlAccessList>(`${p(slug)}/shl/${encodeURIComponent(id)}/access`);
export const revokeLink = (slug: string, id: Uuid) => del<ShlRevoked>(`${p(slug)}/shl/${encodeURIComponent(id)}`);
