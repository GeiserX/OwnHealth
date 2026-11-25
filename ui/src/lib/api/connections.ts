// v1.0 the Connections screen (slice 55): the health list it reads (routes/inboxes.rs) and the
// connector routes it calls (routes/connectors.rs). The launch from the add wizard is slice 19.
import { del, get, post } from './core';
import type { ConnectionRow } from '#lib/logic/connections';
import type { BrandsFound, BrandsOpened, Uuid } from './types';

/** How a disconnect ended at the vendor; `revoke_page` is the vendor's own page when ours could not finish it, and always for Blue Button (Medicare lists the app until it is removed there). */
export type Revocation = 'revoked' | 'not_offered' | 'failed' | 'no_token';
export interface DisconnectOut {
	id: Uuid;
	revocation: Revocation;
	revoke_page: string | null;
}

/** Every source the person may see, with its health and the connection fields. */
export const listSourceHealth = () => get<ConnectionRow[]>('/inboxes/health');
/** Where to send the browser for a new grant on the same connection. */
export const reconnectConnector = (id: Uuid) => post<{ authorize_url: string }>(`/connectors/${id}/reconnect`);
/** Revoke at the vendor when it offers that, then delete the tokens and the cursor. */
export const disconnectConnector = (id: Uuid) => del<DisconnectOut>(`/connectors/${id}`);

/** The US connection wizard was opened: makes the household's directory source on the first call. */
export const openBrands = () => post<BrandsOpened>('/brands');
/** Searches the directory by facility name; never makes the source. */
export const searchBrands = (q: string, limit?: number) => get<BrandsFound>("/brands", { q, limit });
