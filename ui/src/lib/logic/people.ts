// Pure helpers for the people and access screens (Family, Settings, MCP & access, the step-up dialog).
// No runes and no t(): the pages pass words in, so these stay testable with node --test.

/**
 * Ask for the password until it is right or the person cancels. `ask` gets the error of the last try
 * (null the first time) and resolves null on Cancel. `wrong` turns a failed `finish` into the words to
 * show, or null when the failure is not a wrong password: that one is thrown, the dialog closes and the
 * caller shows it next to the control.
 */
export async function askUntilRight(
	ask: (error: string | null) => Promise<string | null>,
	finish: (pw: string) => Promise<void>,
	wrong: (e: unknown) => string | null
): Promise<boolean> {
	let error: string | null = null;
	for (;;) {
		const pw = await ask(error);
		if (pw === null) return false;
		try {
			await finish(pw);
			return true;
		} catch (e) {
			error = wrong(e);
			if (error === null) throw e;
		}
	}
}

/**
 * The column heads of the grants matrix: the first name, or the full name when two profiles share a
 * first name (two 'Lucía' columns cannot be told apart). `dup` tells the page to add the slug.
 */
export function matrixHeads(profiles: { slug: string; display_name: string }[]): { slug: string; label: string; dup: boolean }[] {
	const first = (n: string) => n.trim().split(/\s+/)[0] ?? n;
	const count = new Map<string, number>();
	for (const p of profiles) count.set(first(p.display_name).toLowerCase(), (count.get(first(p.display_name).toLowerCase()) ?? 0) + 1);
	return profiles.map((p) => {
		const dup = (count.get(first(p.display_name).toLowerCase()) ?? 0) > 1;
		return { slug: p.slug, label: dup ? p.display_name : first(p.display_name), dup };
	});
}

/**
 * A member's row in the grants matrix: the first name, or the full name when that first name also
 * belongs to another member or to a profile of someone else (an invitee 'Carmen' next to the profile
 * column 'Carmen'); the signed-in member's row ends in ' (you)'.
 */
export function matrixRowLabel(
	m: { user_id: string; display_name: string; is_me?: boolean },
	members: { user_id: string; display_name: string }[],
	profiles: { display_name: string }[],
	you: string
): string {
	const first = (n: string) => (n.trim().split(/\s+/)[0] ?? n).toLocaleLowerCase();
	const same = (a: string, b: string) => a.trim().replace(/\s+/g, ' ').toLocaleLowerCase() === b.trim().replace(/\s+/g, ' ').toLocaleLowerCase();
	const f = first(m.display_name);
	const clash =
		members.some((o) => o.user_id !== m.user_id && first(o.display_name) === f) ||
		profiles.some((p) => first(p.display_name) === f && !same(p.display_name, m.display_name));
	const label = clash ? m.display_name.trim() : (m.display_name.trim().split(/\s+/)[0] ?? m.display_name);
	return m.is_me ? `${label} (${you})` : label;
}

/** True when a profile already carries this name (case and spacing ignored). A note, never a block. */
export function nameTaken(name: string, profiles: { display_name: string }[]): boolean {
	const n = name.trim().replace(/\s+/g, ' ').toLocaleLowerCase();
	return !!n && profiles.some((p) => p.display_name.trim().replace(/\s+/g, ' ').toLocaleLowerCase() === n);
}

/** The origin of a link when it differs from the page's own (an OWNHEALTH_BASE_URL the person may not reach), else null. */
export function foreignOrigin(url: string, here: string): string | null {
	try {
		const o = new URL(url).origin;
		return o === here ? null : o;
	} catch {
		return null;
	}
}

/** Today as YYYY-MM-DD in local time, for the max of a birth date field. */
export function todayIso(now = new Date()): string {
	const pad = (n: number) => String(n).padStart(2, '0');
	return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

/** OAuth consents that still give access: not revoked and not expired. */
export function liveConsents<T extends { revoked_at: string | null; expires_at: string }>(list: T[] | null | undefined, now = new Date()): T[] {
	return (list ?? []).filter((c) => !c.revoked_at && new Date(c.expires_at) > now);
}

/** True when removing this passkey leaves no way in but the recovery codes. */
export function lastSignIn(passkeys: number, password: boolean, oidc: boolean): boolean {
	return passkeys <= 1 && !password && !oidc;
}

// ---- the audit log on MCP & access ----

export type AuditRow = { via: string; actor: string; action: string; tool: string | null; result_kind: string | null; rows_out: number | null; bytes_out: number | null };

/** 'agents' keeps what agents did over MCP; 'all' keeps every row, page views included. */
export function auditRows<T extends { via: string }>(rows: T[] | null | undefined, mode: 'agents' | 'all'): T[] {
	return mode === 'all' ? (rows ?? []) : (rows ?? []).filter((a) => a.via === 'mcp');
}

/** The dict key and pill tone of a result: a budget cut is expected, a denial or an error is not. */
export function auditResult(kind: string | null): { key: string; tone: 'p-ok' | 'p-watch' | 'p-out' | 'p-info' } {
	const k = kind ?? 'ok';
	if (k === 'ok') return { key: 'i2_res_ok', tone: 'p-ok' };
	if (k === 'budget_cut') return { key: 'i2_res_budget_cut', tone: 'p-watch' };
	if (k === 'queued') return { key: 'i2_res_queued', tone: 'p-info' };
	if (k === 'denied') return { key: 'i2_res_denied', tone: 'p-out' };
	if (k === 'error') return { key: 'i2_res_error', tone: 'p-out' };
	// A finding held back by the minor or sensitive rule is the rule working, not a failure.
	if (k.startsWith('withheld_')) return { key: 'i2_res_' + k, tone: 'p-info' };
	return { key: 'i2_res_' + k, tone: 'p-out' };
}

/**
 * Who made the call, for display: the server appends ' (token)' to a token's label, and an OAuth app's
 * token carries the app's address; the app's own name is what Settings shows, so use it.
 * Returns the label and the raw actor for a title.
 */
export function auditActor(actor: string, apps: { client_id: string; name: string }[]): { label: string; raw: string } {
	const bare = actor.replace(/\s*\(token\)\s*$/, '');
	const app = apps.find((a) => a.client_id === bare);
	return { label: app?.name || bare, raw: actor };
}

/** What an MCP row did (its tool), or the action of any other row (token_create, oauth_approve). */
export function auditWhat(a: { via: string; tool: string | null; action: string }): string {
	return a.via === 'mcp' ? (a.tool ?? a.action) : a.action;
}

/** The dict key of what a page or app row did ('i2_act_consent_change'); null for an agent's tool name. */
export function auditActionKey(a: { via: string; tool: string | null; action: string }): string | null {
	if (a.via === 'mcp' && a.tool) return null;
	return 'i2_act_' + a.action.replace(/[^a-z0-9]+/gi, '_');
}

type Words = { t: (key: string) => string; scope: (scope: string) => string; n: (key: string, n: number) => string };
const word = (w: Words, key: string): string | null => {
	const s = w.t(key);
	return s === key ? null : s;
};
const yesNo = (v: string, w: Words) => (v === 'True' ? w.t('i2_on') : w.t('i2_off')).toLocaleLowerCase();

/**
 * The resources column in plain words: a consent change as 'Show in the app: on', a lock change as the
 * locked categories, a new token or app as its counts, else what was touched ('Vaccine ×2'). Other
 * summaries pass through; a hashed argument ('h:…') is never shown.
 */
export function auditDetail(
	a: { action: string; args_summary?: string | null; resource_type?: string | null; resource_ids?: string[] | null },
	w: Words
): string {
	const args = a.args_summary ?? '';
	if (a.action === 'consent_change') {
		const m = /^ui=(True|False|None)[ ;]agents=(True|False|None)$/.exec(args);
		if (m) {
			const parts = [m[1] !== 'None' ? `${w.t('i2_sens_ui')}: ${yesNo(m[1], w)}` : null, m[2] !== 'None' ? `${w.t('i2_sens_ag')}: ${yesNo(m[2], w)}` : null];
			const s = parts.filter(Boolean).join(' · ');
			if (s) return s;
		}
	}
	if (a.action === 'lock_change') {
		const locked = args.split(',').filter(Boolean);
		return `${w.t('i2_lock_on')}: ${locked.length ? locked.map(w.scope).join(', ') : w.t('i2_ad_nothing')}`;
	}
	// A new token or an approved app: 'scopes=read:labs,read:vitals;profiles=2;days=30'.
	const grant = /^scopes=([^;]*);profiles=(\d+);days=(\d+)$/.exec(args);
	if (grant) {
		const scopes = grant[1].split(',').filter(Boolean).length;
		return [w.n('i2_ad_scopes', scopes), w.n('i2_ad_profiles', Number(grant[2])), w.n('i2_ad_days', Number(grant[3]))].join(' · ');
	}
	if (args && !args.startsWith('h:')) return args;
	const type = a.resource_type;
	if (!type) return '—';
	const n = a.resource_ids?.length ?? 0;
	const name = word(w, 'i2_rt_' + type) ?? word(w, 'me_k_' + type) ?? type;
	return n > 1 ? `${name} ×${n}` : name;
}

/** What went out: rows when the tool counts rows, else the size when bytes left but no rows were counted. */
export function auditOut(a: { rows_out: number | null; bytes_out: number | null }): { rows: number | null } | { kb: number } {
	if (!a.rows_out && a.bytes_out && a.bytes_out > 0) return { kb: a.bytes_out / 1000 };
	return { rows: a.rows_out };
}

/** Scopes in the order the token form lists them: plain reads, document text, the risky ones, then write. */
export const SCOPE_ORDER = [
	'read:summary',
	'read:labs',
	'read:vitals',
	'read:wearables',
	'read:records',
	'read:genome',
	'read:documents_text',
	'read:genome_raw',
	'read:sensitive',
	'write:propose'
];

export function byScopeOrder<T>(items: T[], scopeOf: (x: T) => string): T[] {
	const rank = (s: string) => {
		const i = SCOPE_ORDER.indexOf(s);
		return i < 0 ? SCOPE_ORDER.length : i;
	};
	return [...items].sort((a, b) => rank(scopeOf(a)) - rank(scopeOf(b)));
}

/** Items in the order of a reference list of ids (the household order); unknown ids go last, in their own order. */
export function byOrder<T>(items: T[], idOf: (x: T) => string, order: string[]): T[] {
	const rank = (id: string) => {
		const i = order.indexOf(id);
		return i < 0 ? order.length : i;
	};
	return [...items].sort((a, b) => rank(idOf(a)) - rank(idOf(b)));
}

/** The first problem of the new token form, as a dict key, or null. */
export function tokenProblem(label: string, profiles: string[], scopes: string[], days: number | null | undefined): string | null {
	if (!label.trim()) return 'i2_tok_need_label';
	if (!profiles.length || !scopes.length) return 'i2_tok_need';
	if (days == null || !Number.isInteger(days) || days < 1 || days > 365) return 'i2_tok_need_days';
	return null;
}
