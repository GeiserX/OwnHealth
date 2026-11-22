// The grouped item a connector pull files for resource types the app does not show yet
// (fhir_unmapped_summary): the review page names the type, the count and the connection, in es and en,
// and never prints a raw key.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fhirTileSource, unmappedSummary } from '../src/lib/logic/inbox.ts';
import { dict } from '../src/lib/i18n/dict.ts';

const item = (payload: Record<string, unknown>, kind = 'fhir_unmapped_summary') => ({ kind, payload });

test('a summary item gives its type, count and connection', () => {
	const p = { resource_type: 'ExplanationOfBenefit', count: 300, source: { id: 'x', kind: 'bluebutton', label: 'Medicare' } };
	assert.deepEqual(unmappedSummary(item(p)), { type: 'ExplanationOfBenefit', count: 300, source: 'Medicare' });
	assert.deepEqual(unmappedSummary(item({ ...p, source: null })), { type: 'ExplanationOfBenefit', count: 300, source: '' });
});

test('any other item, or a payload without a type and a whole count, gives nothing', () => {
	assert.equal(unmappedSummary(item({ resource_type: 'CarePlan', count: 1 }, 'fhir_unmapped')), null);
	for (const bad of [{ count: 3 }, { resource_type: 'Coverage' }, { resource_type: 'Coverage', count: 0 }, { resource_type: 'Coverage', count: 1.5 }, { resource_type: 'Coverage', count: '3' }]) {
		assert.equal(unmappedSummary(item(bad)), null, JSON.stringify(bad));
	}
});

test('every summary text exists in es and en, with the count in the plural and the singular', () => {
	const en = dict.en as Record<string, string>;
	const es = dict.es as Record<string, string>;
	for (const k of ['i2_kind_fhir_unmapped_summary', 'i2_fus_t', 'i2_fus_t_one', 'i2_fus_d', 'i2_fus_open']) {
		assert.ok(en[k] && es[k] && en[k] !== es[k], k);
	}
	for (const d of [en, es]) {
		assert.match(d.i2_fus_t, /\{n\}.*\{type\}/);
		assert.match(d.i2_fus_t_one, /1 .*\{type\}/);
		assert.match(d.i2_fus_t, /\{source\}/);
	}
});

test('the FHIR tile names the newest FHIR document, a pull’s bundle too: a pull runs no import job', () => {
	const pull = { id: 'pull-doc' };
	// no job at all: the pull's document, never "nothing imported yet", open items or not
	assert.deepEqual(fhirTileSource(null, pull), { job: null, doc: pull });
	// a file job that read another, older document: the pull
	const job = { state: 'done', document_id: null, result: { document_id: 'file-doc' } };
	assert.deepEqual(fhirTileSource(job, pull), { job: null, doc: pull });
	// the job that read the newest document is named (a FHIR job's result names it), and so is one that still runs
	const read = { state: 'done', document_id: null, result: { document_id: 'pull-doc', accepted: 3 } };
	assert.deepEqual(fhirTileSource(read, pull), { job: read, doc: null });
	const named = { state: 'done', document_id: 'pull-doc', result: null };
	assert.deepEqual(fhirTileSource(named, pull), { job: named, doc: null });
	for (const state of ['queued', 'running']) {
		const runs = { state, document_id: null, result: null };
		assert.deepEqual(fhirTileSource(runs, pull), { job: runs, doc: null }, state);
	}
	// a failed file job does not hide the pulls after it
	assert.deepEqual(fhirTileSource({ state: 'failed', document_id: null, result: null }, pull), { job: null, doc: pull });
	assert.deepEqual(fhirTileSource(job, null), { job, doc: null });
	assert.deepEqual(fhirTileSource(null, null), { job: null, doc: null });
});
