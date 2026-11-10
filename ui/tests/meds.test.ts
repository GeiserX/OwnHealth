// The Medications screen: which medicines are current, the history grouped per medicine, and the code chips.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { codeChips, historyGroups, isCurrent, type MedRecord } from '../src/lib/logic/meds.ts';

const rec = (id: string, detail: Record<string, unknown>, extra: Partial<MedRecord> = {}): MedRecord => ({
	id,
	title: String(detail.name ?? ''),
	detail,
	status: 'current',
	status_reason: null,
	supersedes: null,
	recorded_at: '2026-11-01T10:00:00Z',
	...extra
});

test('a medicine is current between its start and its stop, as the server counts it', () => {
	const today = '2026-11-10';
	assert.equal(isCurrent(rec('a', { name: 'A', med_status: 'active', started: '2026-03-10' }), today), true);
	assert.equal(isCurrent(rec('b', { name: 'B', med_status: 'stopped', started: '2022-06-20', stopped: '2025-12-01' }), today), false);
	assert.equal(isCurrent(rec('c', { name: 'C', med_status: 'active', started: '2026-12-01' }), today), false, 'starts later');
	assert.equal(isCurrent(rec('d', { name: 'D', med_status: 'on_hold', started: '2026-01-01' }), today), false);
	assert.equal(isCurrent(rec('e', { name: 'E', med_status: 'active' }), today), true, 'no dates');
	assert.equal(isCurrent(rec('f', { name: 'F', med_status: 'active' }, { status: 'superseded' }), today), false);
	assert.equal(isCurrent(rec('g', { name: 'G', med_status: 'stopped', started: '2026-01-01', stopped: '2026-12-01' }), today), true, 'stops later');
});

test('the history groups the pause and the restart of one medicine, newest first', () => {
	const groups = historyGroups(
		[
			rec('omep', { name: 'Omeprazole', ingredient: 'omeprazole', started: '2026-04-01', med_status: 'active' }),
			rec('old', { name: 'Atorvastatin', ingredient: 'atorvastatin', started: '2022-06-20', stopped: '2025-12-01', med_status: 'stopped', reason_text: 'paused' }),
			rec('new', { name: 'Atorvastatin', ingredient: 'Atorvastatin ', started: '2026-03-10', med_status: 'active' }),
			rec('v1', { name: 'Vitamin D3', kind: 'supplement', started: '2025-10-01', med_status: 'active' }, { status: 'superseded', status_reason: 'dose corrected' })
		],
		'2026-11-10'
	);
	assert.deepEqual(
		groups.map((g) => [g.title, g.entries.map((e) => e.id), g.current]),
		[
			['Omeprazole', ['omep'], true],
			['Atorvastatin', ['new', 'old'], true],
			['Vitamin D3', ['v1'], false]
		]
	);
});

test('code chips show the primary code first and every received code once', () => {
	const chips = codeChips({
		code_system: 'rxnorm',
		code: '860975',
		codes: [
			{ system: 'https://example.org/fhir/sid/national-drug-code', code: '000000-0', display: 'Metformina 850 mg' },
			{ system: 'http://www.nlm.nih.gov/research/umls/rxnorm', code: '860975' },
			{ system: 'http://www.whocc.no/atc', code: 'A10BA02' }
		]
	});
	assert.deepEqual(
		chips.map((c) => `${c.label} ${c.code}`),
		['RxNorm 860975', 'example.org 000000-0', 'ATC A10BA02']
	);
	assert.equal(chips[1].display, 'Metformina 850 mg');
	assert.deepEqual(codeChips({}), []);
	assert.deepEqual(codeChips({ codes: 'not a list' }), []);
});
