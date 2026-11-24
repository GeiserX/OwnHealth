// v1.0 the Connections screen (slice 55): the health list it reads (routes/inboxes.rs) and the
// connector routes it calls (routes/connectors.rs). The launch from the add wizard is slice 19.
import { del, get, post } from './core';
import type { ConnectionRow } from '#lib/logic/connections';
import type { Uuid } from './types';

/** How a disconnect ended at the vendor; `revoke_page` is the vendor's own page when ours could not finish it. */
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

/** One vendor's copy of the US endpoint directory (routes/brands.rs). */
export interface BrandFeed {
	vendor: string;
	fetched_at: string | null;
	entries: number;
	stale: boolean;
}
/** The household's US endpoint directory; null before the wizard was first opened. */
export interface BrandDirectory {
	source_id: Uuid;
	enabled: boolean;
	last_run_at: string | null;
	feeds: BrandFeed[];
	stale: boolean;
	ready: boolean;
}
/** A facility whose name holds every word of the query. */
export interface BrandHit {
	name: string;
	endpoint: string;
	state: string | null;
	vendor: string;
}
export interface BrandsOpened {
	created: boolean;
	directory: BrandDirectory | null;
}
export interface BrandsFound {
	directory: BrandDirectory | null;
	results: BrandHit[];
}

/** The US connection wizard was opened: makes the household's directory source on the first call. */
export const openBrands = () => post<BrandsOpened>('/brands');
/** Searches the directory by facility name; never makes the source. */
export const searchBrands = (q: string, limit?: number) => get<BrandsFound>("/brands", { q, limit });
