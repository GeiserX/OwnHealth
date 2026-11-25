import { ApiError, get } from '#lib/api/core';
import type { PageLoad } from './$types';

// Without a redirect URI while origin_fix names the setting Open Wearables would write differently.
type Provider = { id: string; mcp: 'served' | 'withheld'; redirect_uri: string | null };
type Setup = {
	api_base_url: string | null;
	providers: Provider[];
	origin_fix: { setting: string; value: string | null } | null;
};

export const load: PageLoad = async ({ params }) => {
	const id = params.provider ?? null;
	try {
		return { id, setup: await get<Setup>('/wearables/setup'), forbidden: false };
	} catch (e) {
		// only an owner or guardian reads the guide (routes/ow_proxy.rs)
		return { id, setup: null, forbidden: e instanceof ApiError && e.status === 403 };
	}
};
