import { ApiError, get } from '#lib/api/core';
import type { PageLoad } from './$types';

type Provider = { id: string; mcp: 'served' | 'withheld'; redirect_uri: string };
type Setup = { api_base_url: string; providers: Provider[] };

export const load: PageLoad = async ({ params }) => {
	const id = params.provider ?? null;
	try {
		return { id, setup: await get<Setup>('/wearables/setup'), forbidden: false };
	} catch (e) {
		// only an owner or guardian reads the guide (routes/ow_proxy.rs)
		return { id, setup: null, forbidden: e instanceof ApiError && e.status === 403 };
	}
};
