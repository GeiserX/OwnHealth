// Unit tests for the intake screens (Import & review, Inbox, Share): review row checks, the agent
// proposal's first ticks, a new source's status, the source form's http check, who may own a source,
// the 'Others' grouping, the inbox summary tile and the share file filter.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
	conflictValue,
	inboxSummary,
	manageable,
	othersGrouped,
	proposalDefaultKeep,
	repeatsLabel,
	reviewChecks,
	shareAccepts,
	sourceBody,
	emptyForm,
	sourceStatus,
	type ProposalRow
} from '../src/lib/logic/inbox.ts';
import { errKey } from '../src/lib/logic/errors.ts';

test('review checks: one line per reason, the conflict keeps the value it found', () => {
	const checks = [
		{ ok: true, name: 'range', reason: null, detail: null },
		{ ok: false, name: 'conflict', reason: 'conflict', detail: 'another value for this analyte and day is accepted (d6aec801-3016)' },
		{ ok: false, name: 'conflict', reason: 'conflict', detail: 'another document holds 231 for this date' },
		{ ok: false, name: 'grounding', reason: 'not_grounded', detail: 'value token not inside the row box' }
	];
	const out = reviewChecks(checks, ['not_grounded']);
	assert.equal(out.length, 1);
	assert.equal(out[0].reason, 'conflict');
	assert.equal(out[0].value, '231');
	assert.equal(conflictValue('another document holds None for this date'), null);
	assert.equal(conflictValue('another value for this analyte and day is accepted (x)'), null);
});

test('review checks: a detail that only repeats the label is dropped', () => {
	assert.equal(repeatsLabel('no LOINC code for this name', 'No LOINC code for this name'), true);
	assert.equal(repeatsLabel('iron 162 mg/dL is above the hard limit', 'Value outside the plausible range'), false);
});

const row = (o: Partial<ProposalRow>): ProposalRow => ({
	index: 0,
	name: 'Glucosa',
	value: '94',
	unit: 'mg/dL',
	page: 1,
	bbox: [0, 0, 1, 1],
	grounding: 'grounded',
	problems: [],
	usable: true,
	...o
});

test('agent proposal: a duplicate or conflicting row starts unticked', () => {
	const rows = [
		row({ index: 0 }),
		row({ index: 1, problems: [{ reason: 'duplicate', detail: null }] }),
		row({ index: 2, problems: [{ reason: 'conflict', detail: null }] }),
		row({ index: 3, problems: [{ reason: 'implausible', detail: null }] })
	];
	assert.deepEqual(proposalDefaultKeep(rows), [0, 3]);
});

test('a source that has not had its first run yet is No data, not Stale', () => {
	const now = new Date('2026-11-10T10:00:00Z');
	const base = { enabled: true, last_success_at: null, last_error_at: null, consecutive_failures: 0, stale: true, last_run_at: null, poll_seconds: 300 };
	assert.equal(sourceStatus({ ...base, created_at: '2026-11-10T09:58:00Z' }, now), 'nodata');
	assert.equal(sourceStatus({ ...base, created_at: '2026-11-10T09:00:00Z' }, now), 'stale');
	assert.equal(sourceStatus({ ...base, created_at: '2026-11-10T09:58:00Z', last_run_at: '2026-11-10T09:59:00Z' }, now), 'stale');
});

test('the source form refuses plain http for a public address unless the LAN box is ticked', () => {
	const f = { ...emptyForm('nightscout'), label: 'NS', profile_id: 'p1', url: 'http://ns.example', secret: 's' };
	assert.deepEqual(sourceBody(f), { error: 'fu_err_http' });
	assert.ok('body' in sourceBody({ ...f, allow_private: true }));
	assert.ok('body' in sourceBody({ ...f, url: 'https://ns.example' }));
	assert.deepEqual(sourceBody({ ...emptyForm('paperless'), label: 'P', profile_id: 'p1', url: 'http://10.0.0.2:8000', secret: 's' }), { error: 'fu_err_http' });
});

test('only profiles a person manages can own a source', () => {
	const ps = [
		{ id: 'a', role: 'owner' },
		{ id: 's', role: 'viewer' },
		{ id: 'l', role: 'guardian' },
		{ id: 'c', role: 'caregiver' }
	];
	assert.deepEqual(
		manageable(ps).map((p) => p.id),
		['a', 'l']
	);
});

test("'Others' shows one line per document for its extracted rows", () => {
	const items = [
		{ id: '1', kind: 'candidate_row', document_id: 'd1' },
		{ id: '2', kind: 'agent_proposal', document_id: null },
		{ id: '3', kind: 'candidate_row', document_id: 'd1' },
		{ id: '4', kind: 'candidate_row', document_id: 'd2' },
		{ id: '5', kind: 'pending_document', document_id: 'd3' }
	];
	const g = othersGrouped(items);
	assert.deepEqual(
		g.map((x) => (x.rows ? `${x.document_id}:${x.rows.length}` : x.item.id)),
		['d1:2', '2', 'd2:1', '5']
	);
});

test('inbox summary tile: counts sources, failing and held mail; nothing while not loaded', () => {
	assert.equal(inboxSummary(null), null);
	const s = (o: object) => ({ kind: 'email', enabled: true, consecutive_failures: 0, quarantined: 0, ...o });
	assert.deepEqual(inboxSummary([s({ consecutive_failures: 2, quarantined: 1 }), s({ kind: 'share', quarantined: 5 }), s({ enabled: false, consecutive_failures: 3 })]), {
		sources: 3,
		failing: 1,
		held: 1
	});
});

test('share: only PDFs and images are sent', () => {
	assert.equal(shareAccepts({ name: 'a.pdf', type: 'application/pdf' }), true);
	assert.equal(shareAccepts({ name: 'scan.JPG', type: '' }), true);
	assert.equal(shareAccepts({ name: 'p.heic', type: 'image/heic' }), true);
	assert.equal(shareAccepts({ name: 'notes.txt', type: 'text/plain' }), false);
});

test('a source that cannot reach its server says so in plain words', () => {
	const has = (k: string) => k === 'err_unreachable' || k === 'err_refused';
	assert.equal(errKey(null, 'RuntimeError: request to 127.0.0.1 failed: error sending request', has), 'err_unreachable');
	assert.equal(errKey(null, 'ImapError: connect: Connection refused (os error 61)', has), 'err_refused');
});
