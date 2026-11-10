// Unit tests for the v0.2 UI helpers (lane F-ui): source status, the source form body, sender
// patterns, grounding flags, agent context, the duplicate banner and the consent defaults.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
	agentContext,
	cgmBands,
	consentDefaults,
	copyText,
	consentProblem,
	emptyForm,
	formOf,
	heldTotal,
	nearDuplicateOf,
	paperlessLink,
	portForTls,
	proposalAcceptEdits,
	proposalDefaultKeep,
	proposalRows,
	quarantineReason,
	rowGrounding,
	senderPattern,
	shareTone,
	sourceBody,
	sourceStatus,
	sourceWhere,
	tokenButtonKey
} from '../src/lib/logic/inbox.ts';

const h = (o: Partial<Parameters<typeof sourceStatus>[0]>) => ({
	enabled: true,
	last_success_at: '2026-11-07T06:00:00Z',
	last_error_at: null,
	consecutive_failures: 0,
	stale: false,
	...o
});

test('source status: failing beats stale, never-ran is no data, disabled is off', () => {
	assert.equal(sourceStatus(h({})), 'ok');
	assert.equal(sourceStatus(h({ enabled: false, consecutive_failures: 3 })), 'off');
	assert.equal(sourceStatus(h({ consecutive_failures: 2, last_error_at: '2026-11-07T07:00:00Z', stale: true })), 'out');
	assert.equal(sourceStatus(h({ last_success_at: null, consecutive_failures: 1, last_error_at: '2026-11-07T07:00:00Z' })), 'out');
	assert.equal(sourceStatus(h({ stale: true })), 'stale');
	assert.equal(sourceStatus(h({ last_success_at: null })), 'nodata');
	// An old failure followed by a success is OK again.
	assert.equal(sourceStatus(h({ consecutive_failures: 0, last_error_at: '2026-11-01T07:00:00Z' })), 'ok');
});

test('email form builds the IMAP config, seals the password, and refuses gaps', () => {
	const f = { ...emptyForm('email'), label: 'Correo de salud', host: 'imap.rivera.example', username: 'salud@rivera.example', secret: 'pw' };
	const r = sourceBody(f);
	assert.ok('body' in r);
	assert.equal(r.body.secret, 'pw');
	assert.equal(r.body.poll_seconds, 60);
	assert.deepEqual(r.body.config, {
		host: 'imap.rivera.example',
		port: 993,
		tls: 'implicit',
		username: 'salud@rivera.example',
		folder: 'INBOX',
		done_folder: 'OwnHealth/Done',
		trusted_authserv_id: null
	});
	assert.equal(r.body.profile_id, null, 'sender rules pick the profile per message');
	// The password never lands in config.
	assert.ok(!JSON.stringify(r.body.config).includes('pw'));
	assert.deepEqual(sourceBody({ ...f, secret: '' }), { error: 'fu_err_secret' });
	assert.ok('body' in sourceBody({ ...f, secret: '' }, true), 'editing keeps the stored secret');
	assert.deepEqual(sourceBody({ ...f, host: ' ' }), { error: 'fu_err_imap' });
	assert.deepEqual(sourceBody({ ...f, port: '70000' }), { error: 'fu_err_port' });
	assert.deepEqual(sourceBody({ ...f, poll_minutes: '0' }), { error: 'fu_err_poll' });
	assert.deepEqual(sourceBody({ ...f, label: '' }), { error: 'fu_err_label' });
});

test('push and device kinds need a profile; URLs must be http(s)', () => {
	assert.deepEqual(sourceBody({ ...emptyForm('health_auto_export'), label: 'iPhone' }), { error: 'fu_err_profile' });
	const hae = sourceBody({ ...emptyForm('health_auto_export'), label: 'iPhone', profile_id: 'p1' });
	assert.ok('body' in hae);
	assert.equal(hae.body.poll_seconds, null, 'a push source is not polled');
	assert.equal(hae.body.secret, undefined);
	const ns = { ...emptyForm('nightscout'), label: 'CGM Lucía', profile_id: 'p3', secret: 'tok', url: 'javascript:alert(1)' };
	assert.deepEqual(sourceBody(ns), { error: 'fu_err_url' });
	const ok = sourceBody({ ...ns, url: 'https://ns.rivera.example/', allow_private: true });
	assert.ok('body' in ok);
	assert.deepEqual(ok.body.config, { base_url: 'https://ns.rivera.example', allow_private: true });
	const pl = sourceBody({ ...emptyForm('paperless'), label: 'Paperless', profile_id: 'p1', url: 'http://paperless.lan:8000', secret: 't', tag: '', allow_private: true });
	assert.ok('body' in pl);
	assert.equal(pl.body.config.health_tag, 'health');
	assert.equal(pl.body.poll_seconds, 900);
});

test('folder path stays inside the inbox directory', () => {
	assert.deepEqual(sourceBody({ ...emptyForm('folder'), label: 'Escáner', path: '../etc' }), { error: 'fu_err_path' });
	const r = sourceBody({ ...emptyForm('folder'), label: 'Escáner', path: '/scans/' });
	assert.ok('body' in r);
	assert.equal(r.body.config.subdir, 'scans');
});

test('formOf round-trips a stored source without its secret', () => {
	const f = { ...emptyForm('email'), label: 'Correo', host: 'imap.rivera.example', username: 'u', secret: 's', poll_minutes: '10' };
	const r = sourceBody(f);
	assert.ok('body' in r);
	const back = formOf({ ...r.body, profile_id: null });
	assert.equal(back.secret, '');
	assert.equal(back.host, 'imap.rivera.example');
	assert.equal(back.poll_minutes, '10');
	const again = sourceBody(back, true);
	assert.ok('body' in again);
	assert.deepEqual(again.body.config, r.body.config);
});

test('sender patterns: address from a display name, domain, lower case; junk refused', () => {
	assert.equal(senderPattern('LabNorte Resultados <Resultados@LabNorte.example>'), 'resultados@labnorte.example');
	assert.equal(senderPattern('@labnorte.example'), 'labnorte.example');
	assert.equal(senderPattern('labnorte.example'), 'labnorte.example');
	assert.equal(senderPattern('not an address'), null);
	assert.equal(senderPattern('a@b'), null);
});

test('grounding reads the column or the reason codes', () => {
	assert.equal(rowGrounding({ reasons: [], payload: { grounding: 'grounded' } }), 'grounded');
	assert.equal(rowGrounding({ reasons: ['not_grounded'], payload: {} }), 'ungrounded');
	assert.equal(rowGrounding({ reasons: [], payload: { grounding: 'ungrounded' } }), 'ungrounded');
	assert.equal(rowGrounding({ reasons: ['ungrounded_no_word_layer'], payload: {} }), 'no_word_layer');
	assert.equal(rowGrounding({ reasons: [], payload: { grounding: 'ungrounded_no_word_layer' } }), 'no_word_layer');
	assert.equal(rowGrounding({ reasons: ['loinc_unknown'], payload: {} }), null);
});

test('agent context merges the item and payload lists without repeats', () => {
	const c = agentContext({ payload: { context_documents: ['d1'], based_on: ['src_a'] }, context_src_ids: ['src_a', 'src_b'], context_documents: ['d1', 'd2'] });
	assert.deepEqual(c, { documents: ['d1', 'd2'], src_ids: ['src_a', 'src_b'] });
	assert.deepEqual(agentContext({ payload: {}, context_src_ids: null }), { documents: [], src_ids: [] });
});

test('duplicate banner finds only the open near_duplicate of this document', () => {
	const items = [
		{ kind: 'candidate_row', document_id: 'd1', status: 'open' },
		{ kind: 'near_duplicate', document_id: 'd2', status: 'open' },
		{ kind: 'near_duplicate', document_id: 'd1', status: 'dismissed' }
	];
	assert.equal(nearDuplicateOf(items, 'd1'), null);
	assert.equal(nearDuplicateOf(items, 'd2'), items[1]);
	assert.equal(nearDuplicateOf(items, null), null);
});

test('consent pre-checks only read:summary and no profile', () => {
	assert.deepEqual(consentDefaults(['read:summary', 'read:labs', 'write:propose']), { scopes: ['read:summary'], profiles: [], days: 90 });
	assert.deepEqual(consentDefaults(['read:labs'], 900), { scopes: [], profiles: [], days: 365 });
	assert.equal(consentProblem(['read:summary'], [], 90), 'fu_oa_need_profile');
	assert.equal(consentProblem([], ['p1'], 90), 'fu_oa_need_scope');
	assert.equal(consentProblem(['read:summary'], ['p1'], 366), 'fu_oa_need_days');
	assert.equal(consentProblem(['read:summary'], ['p1'], 90), null);
});

test('every key the v0.2 screens build at run time exists in es and en', async () => {
	const { dict } = await import('../src/lib/i18n/dict.ts');
	const { ADDABLE, PUSH_KINDS, SECRET_OF } = await import('../src/lib/logic/inbox.ts');
	const kinds = ['email', 'folder', 'paperless', 'share', 'shortcut', 'health_auto_export', 'gadgetbridge', 'nightscout'];
	const scopes = ['read:summary', 'read:labs', 'read:vitals', 'read:wearables', 'read:records', 'read:genome', 'read:genome_raw', 'read:sensitive', 'read:documents_text', 'write:propose'];
	const keys = [
		...kinds.map((k) => 'fu_k_' + k),
		...ADDABLE.flatMap((k) => ['fu_kd_' + k, 'fu_ph_' + k]),
		...PUSH_KINDS.map((k) => 'fu_tk_' + k),
		...Object.values(SECRET_OF).map((s) => 'fu_s_' + s),
		...['ok', 'out', 'stale', 'nodata', 'off'].map((s) => 'fu_st_' + s),
		...['poll', 'push', 'manual'].map((s) => 'fu_trig_' + s),
		...['running', 'ok', 'partial', 'error'].map((s) => 'fu_run_' + s),
		...['sender_not_allowed', 'auth_failed', 'auth_unchecked', 'too_large', 'unsupported_type', 'no_profile'].map((s) => 'fu_qr_' + s),
		...(['email', 'folder', 'nightscout', 'paperless', 'shortcut', 'health_auto_export'] as const).flatMap((k) => [tokenButtonKey(k, false), tokenButtonKey(k, true)]),
		...scopes.map((s) => 'fu_sc_' + s.replace(':', '_'))
	];
	for (const lang of ['en', 'es'] as const) {
		const table = dict[lang] as Record<string, string>;
		const missing = keys.filter((k) => !table[k]);
		assert.deepEqual(missing, [], `${lang} is missing ${missing.join(', ')}`);
	}
});

test('the Paperless link is shown only for an http(s) source_link of kind paperless', () => {
	const url = 'http://paperless.lan:8000/documents/7/details';
	assert.equal(paperlessLink({ source_link: { kind: 'paperless', id: 7, url } }), url);
	assert.equal(paperlessLink({ source_link: { kind: 'paperless', id: 7, url: 'javascript:alert(1)' } }), null);
	assert.equal(paperlessLink({ source_link: { kind: 'email', url } }), null);
	assert.equal(paperlessLink({}), null);
	assert.equal(paperlessLink(null), null);
});

test('source status: a partial last run shows Partial, below Out and above Stale', () => {
	assert.equal(sourceStatus(h({ last_run_status: 'partial' })), 'partial');
	assert.equal(sourceStatus(h({ last_run_status: 'partial', stale: true })), 'partial');
	assert.equal(sourceStatus(h({ last_run_status: 'ok' })), 'ok');
	assert.equal(
		sourceStatus(h({ last_run_status: 'partial', consecutive_failures: 1, last_error_at: '2026-11-07T07:00:00Z' })),
		'out'
	);
	assert.equal(sourceStatus(h({ last_run_status: 'partial', enabled: false })), 'off');
});

test('changing the TLS mode moves a default port, never one the person typed', () => {
	assert.equal(portForTls('implicit', 'starttls', '993'), '143');
	assert.equal(portForTls('starttls', 'implicit', '143'), '993');
	assert.equal(portForTls('implicit', 'none', '993'), '143');
	assert.equal(portForTls('starttls', 'none', '143'), '143');
	assert.equal(portForTls('implicit', 'starttls', '1993'), '1993');
	assert.equal(portForTls('implicit', 'starttls', ''), '143');
});

test('held mail is counted from the server counts, not the first page of the list', () => {
	assert.equal(heldTotal(null), null);
	assert.equal(
		heldTotal([
			{ kind: 'email', quarantined: 140 },
			{ kind: 'email', quarantined: 3 },
			{ kind: 'folder' },
			{ kind: 'email' }
		]),
		143
	);
});

test('editing a source keeps the config keys the form does not show', () => {
	const stored = { base_url: 'http://paperless.lan:8000', health_tag: 'health', allow_private: true, profile_by_tag: { '12': 'lucia-profile' } };
	const f = formOf({ kind: 'paperless', label: 'Paperless', profile_id: 'p1', config: stored, poll_seconds: 900 });
	const r = sourceBody({ ...f, poll_minutes: '30' }, true, stored);
	assert.ok('body' in r);
	assert.deepEqual(r.body.config.profile_by_tag, { '12': 'lucia-profile' }, 'tag routing survives an edit');
	assert.equal(r.body.poll_seconds, 1800);
	// An older `url` key is replaced, never sent next to base_url (the server refuses both at once).
	const ns = { url: 'https://ns.rivera.example', backfill_days: 30, api: 'v3' };
	const n = sourceBody(formOf({ kind: 'nightscout', label: 'CGM', profile_id: 'p3', config: ns, poll_seconds: 300 }), true, ns);
	assert.ok('body' in n);
	assert.deepEqual(n.body.config, { base_url: 'https://ns.rivera.example', allow_private: false, backfill_days: 30, api: 'v3' });
	const mail = { host: 'imap.rivera.example', port: 993, tls: 'implicit', username: 'u', ca_file: '/certs/ca.pem' };
	const m = sourceBody(formOf({ kind: 'email', label: 'Correo', profile_id: null, config: mail, poll_seconds: 60 }), true, mail);
	assert.ok('body' in m);
	assert.equal(m.body.config.ca_file, '/certs/ca.pem');
});

test('source cards read the keys the form writes, and the older ones', () => {
	assert.deepEqual(sourceWhere({ subdir: 'scans', base_url: 'http://p.lan', health_tag: 'health' }), { subdir: 'scans', url: 'http://p.lan', tag: 'health' });
	assert.deepEqual(sourceWhere({ path: 'old', url: 'http://n.lan', tag: '7' }), { subdir: 'old', url: 'http://n.lan', tag: '7' });
	assert.deepEqual(sourceWhere(null), { subdir: '', url: '', tag: '' });
});

test('every inbox kind offers a token: push kinds for the sender, polled kinds for the monitor', () => {
	assert.equal(tokenButtonKey('paperless', false), 'fu_token');
	assert.equal(tokenButtonKey('health_auto_export', true), 'fu_token_new');
	assert.equal(tokenButtonKey('email', false), 'fu_mon_token');
	assert.equal(tokenButtonKey('nightscout', true), 'fu_mon_token_new');
	assert.equal(tokenButtonKey('folder', false), 'fu_mon_token');
});

test('mail held from a mailbox with no authserv-id is unchecked (amber), not a DKIM/SPF failure', () => {
	assert.deepEqual(quarantineReason('auth_failed', { trusted_authserv_id: null }), { key: 'fu_qr_auth_unchecked', tone: 'p-watch' });
	assert.deepEqual(quarantineReason('auth_failed', { trusted_authserv_id: '  ' }), { key: 'fu_qr_auth_unchecked', tone: 'p-watch' });
	assert.deepEqual(quarantineReason('auth_failed', { trusted_authserv_id: 'mx.rivera.example' }), { key: 'fu_qr_auth_failed', tone: 'p-out' });
	assert.deepEqual(quarantineReason('sender_not_allowed', {}), { key: 'fu_qr_sender_not_allowed', tone: 'p-watch' });
});

test('"copied" only when the clipboard took the token', async () => {
	assert.equal(await copyText(undefined, 'x'), false, 'no clipboard on plain http');
	assert.equal(await copyText({ writeText: () => Promise.reject(new Error('denied')) }, 'x'), false);
	let got = '';
	assert.equal(await copyText({ writeText: async (t) => void (got = t) }, 'tok'), true);
	assert.equal(got, 'tok');
});

test('a shared file that will never import is not green', () => {
	assert.equal(shareTone({ outcome: 'unsupported' }).tone, 'p-watch');
	assert.equal(shareTone({ outcome: 'queued' }).tone, 'p-ok');
	assert.equal(shareTone({ outcome: 'already_seen' }).tone, 'p-stale');
	assert.equal(shareTone({ duplicate: true }).key, 'fu_share_dup');
});

test('an invalid CGM summary draws no band (missing is not 0 %)', () => {
	const keys = ['very_low_pct', 'low_pct', 'in_range_pct'] as const;
	assert.equal(cgmBands({ valid: false, very_low_pct: null, low_pct: null, in_range_pct: null }, keys), null);
	assert.deepEqual(cgmBands({ valid: true, very_low_pct: 0, low_pct: 2.5, in_range_pct: 97.5 }, keys), { very_low_pct: 0, low_pct: 2.5, in_range_pct: 97.5 });
	assert.equal(cgmBands({ valid: true, very_low_pct: null, low_pct: 2.5, in_range_pct: 97.5 }, keys), null);
});

// An observations proposal as propose.rs stores it: the agent's rows, each with the server's checks.
const proposal = {
	kind: 'agent_proposal',
	payload: {
		kind: 'observations',
		payload: { doc_id: 'doc_1', rows: [] },
		rows: [
			{ input: { name: 'Glucosa', value: 92, unit: 'mg/dL', page: 1 }, observation: { loinc: '2345-7' }, grounding: 'grounded', bbox: [10, 20, 60, 30], checks: [{ name: 'grounding', ok: true, reason: null }] },
			{ input: { name: 'Hemoglobina', value: '15.1', unit: 'g/dL', page: 2 }, observation: { loinc: '718-7' }, grounding: 'ungrounded', bbox: null, checks: [{ name: 'grounding', ok: false, reason: 'not_grounded', detail: 'value not found on the page word layer' }] },
			{ input: { name: 'TSH', value: 2.1, unit: 'mU/L', page: 1 }, observation: { loinc: '3016-3' }, grounding: 'ungrounded_no_word_layer', bbox: null, checks: [{ name: 'date_in_document', ok: false, reason: 'date_invalid', detail: 'date outside the report' }] },
			{ input: { name: '???', value: 'x', page: 1 }, observation: null, grounding: 'ungrounded', checks: [{ name: 'normalize', ok: false, reason: 'unmapped' }] }
		]
	}
};

test('an observations proposal shows each row with its grounding and problems', () => {
	const rows = proposalRows(proposal);
	assert.ok(rows);
	assert.deepEqual(rows.map((r) => r.grounding), ['grounded', 'ungrounded', 'no_word_layer', 'ungrounded']);
	assert.deepEqual(rows[0].bbox, [10, 20, 60, 30]);
	assert.equal(rows[1].page, 2);
	assert.deepEqual(rows[1].problems, [], 'grounding has its own flag');
	assert.deepEqual(rows[2].problems, [{ reason: 'date_invalid', detail: 'date outside the report' }]);
	assert.equal(rows[3].usable, false);
	// "grounded" without a box is not trusted as grounded
	const odd = proposalRows({ kind: 'agent_proposal', payload: { kind: 'observations', rows: [{ input: {}, observation: {}, grounding: 'grounded' }] } });
	assert.equal(odd?.[0].grounding, 'ungrounded');
	assert.equal(proposalRows({ kind: 'agent_proposal', payload: { kind: 'note', payload: { text: 'x' } } }), null);
	assert.equal(proposalRows({ kind: 'candidate_row', payload: {} }), null);
});

test('accepting a proposal keeps only the ticked rows; ungrounded rows start unticked', () => {
	const rows = proposalRows(proposal) ?? [];
	assert.deepEqual(proposalDefaultKeep(rows), [0]);
	assert.deepEqual(proposalAcceptEdits(rows, [0]), { rows: [0] });
	// A person may tick an ungrounded row after comparing it with the page.
	assert.deepEqual(proposalAcceptEdits(rows, [1, 0, 1]), { rows: [0, 1] });
	// Nothing ticked is no accept at all: an empty list would make the server keep every row.
	assert.equal(proposalAcceptEdits(rows, []), null);
	assert.equal(proposalAcceptEdits(rows, [3]), null, 'a row the server could not read is never sent');
});
