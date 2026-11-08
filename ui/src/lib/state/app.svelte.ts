const browser = typeof window !== 'undefined';
import type { Me, ProfileSummary } from '#lib/api/types';

/** App-wide state: who is logged in, which profile is on screen. */
export const app = $state<{
	me: Me | null;
	profile: string | null; // slug of the profile on screen
	loading: boolean;
	error: string | null;
	unauthenticated: boolean;
}>({ me: null, profile: browser ? localStorage.getItem('oh.profile') : null, loading: true, error: null, unauthenticated: false });

export function setProfile(slug: string): void {
	app.profile = slug;
	if (browser) localStorage.setItem('oh.profile', slug);
}

export function currentProfile(): ProfileSummary | null {
	const list = app.me?.profiles ?? [];
	return list.find((p) => p.slug === app.profile) ?? list.find((p) => p.is_self) ?? list[0] ?? null;
}

// Avatar colours from the mockup, in household order.
export const AVATAR_COLORS = ['#6ea8fe', '#3fb68b', '#e0a84a', '#c39bd3', '#ef6b73', '#8fd3b8'];
