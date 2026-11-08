// The family grant matrix cell for one member and one profile.
type Grant = { user_id: string; profile: string; role: string };
type Member = { user_id: string; own_profile?: string | null };

/**
 * A grant the caller can see gives its role. A member always owns the profile they claimed, even when
 * the caller may not list that grant. Otherwise: '' when the caller manages the profile (so no grant
 * exists), '?' when the caller cannot see that profile's grants.
 */
export function roleCell(grants: Grant[], m: Member, slug: string, callerManages: boolean): string {
	const g = grants.find((x) => x.user_id === m.user_id && x.profile === slug);
	if (g) return g.role;
	if (m.own_profile === slug) return 'owner';
	return callerManages ? '' : '?';
}
