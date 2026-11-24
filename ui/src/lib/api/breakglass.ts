// Break-glass (backend: routes/break_glass.rs): the owner names emergency contacts and cancels or
// revokes their requests; a contact asks with a reason; every owner of the profile hears of each step
// through the in-app notices.
import { del, get, p, post, put } from './core';
import type {
	BreakGlassContact,
	BreakGlassContactIn,
	BreakGlassOne,
	BreakGlassNamed,
	BreakGlassOwnerView,
	BreakGlassRequest,
	Notices,
	Uuid
} from './types';

const bg = (slug: string) => `${p(slug)}/break-glass`;

/** The owner's view of one profile: contacts, open and past requests, whether they have a calendar feed. */
export const getBreakGlass = (slug: string) => get<BreakGlassOwnerView>(bg(slug));
/** Name a contact, or change what they would read (step-up). */
export const putBreakGlassContact = (slug: string, user: Uuid, body: BreakGlassContactIn) =>
	put<BreakGlassContact>(`${bg(slug)}/contacts/${encodeURIComponent(user)}`, body);
/** Remove a contact; their open request ends with it. */
export const removeBreakGlassContact = (slug: string, user: Uuid) =>
	del<{ removed: true; ended: Uuid[] }>(`${bg(slug)}/contacts/${encodeURIComponent(user)}`);
/** One request by its id, for the cancel screen of the calendar link (404 unless the caller owns its profile). */
export const getBreakGlassRequest = (id: Uuid) => get<BreakGlassOne>(`/break-glass/requests/${encodeURIComponent(id)}`);
/** Cancel a request that still waits. */
export const cancelBreakGlass = (slug: string, id: Uuid) =>
	post<BreakGlassRequest>(`${bg(slug)}/requests/${encodeURIComponent(id)}/cancel`);
/** End an open request. */
export const revokeBreakGlass = (slug: string, id: Uuid) =>
	post<BreakGlassRequest>(`${bg(slug)}/requests/${encodeURIComponent(id)}/revoke`);

/** The profiles that named the caller as a contact, each with the caller's last request. */
export const myBreakGlass = () => get<{ items: BreakGlassNamed[] }>('/break-glass');
/** Ask for emergency access (step-up). `profile` is the profile's id or slug. */
export const askBreakGlass = (profile: string, reason: string) =>
	post<BreakGlassRequest>('/break-glass/requests', { profile, reason });

/** The caller's notices and unread count. */
export const getNotices = () => get<Notices>('/notifications');
/** Mark the named notices read, or every one when `ids` is left out. */
export const readNotices = (ids?: Uuid[]) => post<{ read: number }>('/notifications/read', ids ? { ids } : {});
