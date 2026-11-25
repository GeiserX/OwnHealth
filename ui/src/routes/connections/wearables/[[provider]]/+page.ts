import { get } from '#lib/api/core';
import type { PageLoad } from './$types';

type Provider = { id: string; mcp: 'served' | 'withheld'; redirect_uri: string };
type Setup = { api_base_url: string; providers: Provider[] };

export const load: PageLoad = async ({ params }) => {
	const id = params.provider ?? null;
	try {
		return { id, setup: await get<Setup>('/wearables/setup') };
	} catch {
		return { id, setup: null };
	}
};
