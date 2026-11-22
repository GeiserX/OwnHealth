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

test('the FHIR tile names a pull’s bundle: a pull runs no import job', () => {
	const summary = { kind: 'fhir_unmapped_summary', document_id: 'pull-doc', created_at: '2026-11-22T09:00:00Z' };
	const other = { kind: 'candidate_row', document_id: 'pdf-doc', created_at: '2026-11-22T10:00:00Z' };
	// no job at all: the pull's document, never "nothing imported yet"
	assert.deepEqual(fhirTileSource(null, [summary, other]), { job: null, documentId: 'pull-doc' });
	const job = { created_at: '2026-11-21T08:00:00Z', finished_at: '2026-11-21T08:00:05+00:00' };
	// a file job older than the pull's item: still the pull
	assert.deepEqual(fhirTileSource(job, [summary]), { job: null, documentId: 'pull-doc' });
	// a newer job wins, and with no FHIR item the job stays
	const later = { created_at: '2026-11-22T11:00:00Z', finished_at: '2026-11-22T11:00:02Z' };
	assert.deepEqual(fhirTileSource(later, [summary]), { job: later, documentId: null });
	assert.deepEqual(fhirTileSource(job, [other]), { job, documentId: null });
	assert.deepEqual(fhirTileSource(null, []), { job: null, documentId: null });
});
