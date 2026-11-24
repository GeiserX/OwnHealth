// The C-CDA import (slice 20) on the review page: a refused file and the item that lists the sections
// holding text only read in the person's language, never in the server's English.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { CCDA_NARRATIVE, unmappedKeys, unmappedSummary } from '../src/lib/logic/inbox.ts';
import { errKey } from '../src/lib/logic/errors.ts';
import { dict } from '../src/lib/i18n/dict.ts';

const en = dict.en as Record<string, string>;
const es = dict.es as Record<string, string>;

test('a refused C-CDA file reads in the person\'s language', () => {
	const has = (k: string) => k in en;
	for (const why of [
		'the file declares a DOCTYPE, which a C-CDA document never needs',
		'the XML is not well formed: unexpected end of stream',
		'the XML nests deeper than 256 levels',
		'the file is 40 MB, over the 32 MB a C-CDA document may have'
	]) {
		assert.equal(errKey(null, `not a readable C-CDA document: ${why}`, has), 'err_ccda_unreadable', why);
	}
	assert.ok(en.err_ccda_unreadable && es.err_ccda_unreadable && en.err_ccda_unreadable !== es.err_ccda_unreadable);
});

test('a C-CDA file saved as UTF-16, or with no coded body, says what to do in the person\'s language', () => {
	const has = (k: string) => k in en;
	for (const [why, key] of [
		['the file is saved as UTF-16; save it as UTF-8 and upload it again', 'err_ccda_utf16'],
		['the body is a nonXMLBody (a scan or an attached file), with no coded sections to read', 'err_ccda_unstructured']
	]) {
		assert.equal(errKey(null, `not a readable C-CDA document: ${why}`, has), key, why);
		assert.ok(en[key] && es[key] && en[key] !== es[key], key);
	}
	assert.match(en.err_ccda_utf16, /UTF-8/);
	assert.match(es.err_ccda_utf16, /UTF-8/);
});

test('a C-CDA upload\'s items speak of the uploaded file and count entries or sections, a pull\'s stay as they were', () => {
	const item = (resource_type: string, kind = 'ccda') => ({
		kind: 'fhir_unmapped_summary',
		payload: { resource_type, count: 7, source: { kind, label: 'record.xml' } }
	});
	assert.equal(CCDA_NARRATIVE, 'C-CDA narrative section');
	const narrative = unmappedSummary(item(CCDA_NARRATIVE));
	assert.deepEqual(narrative, { type: CCDA_NARRATIVE, count: 7, source: 'record.xml', ccda: 'narrative' });
	// a section the document titles itself keeps the document's own words
	const section = unmappedSummary(item('PROCEDURES (C-CDA)'));
	assert.deepEqual(section, { type: 'PROCEDURES (C-CDA)', count: 7, source: 'record.xml', ccda: 'section' });
	const pull = unmappedSummary(item('Procedure', 'smart'));
	assert.deepEqual(pull, { type: 'Procedure', count: 7, source: 'record.xml' });
	assert.deepEqual(unmappedKeys(pull ?? {}), { title: 'i2_fus_t', note: 'i2_fus_d', open: 'i2_fus_open' });
	assert.deepEqual(unmappedKeys(section ?? {}), { title: 'i2_fus_ccda_t', note: 'i2_fus_ccda_d', open: 'i2_fus_ccda_open' });
	assert.equal(unmappedKeys(narrative ?? {}).title, 'i2_fus_ccda_narrative_t');
	for (const k of ['i2_fus_ccda_t', 'i2_fus_ccda_t_one', 'i2_fus_ccda_narrative_t', 'i2_fus_ccda_narrative_t_one', 'i2_fus_ccda_d', 'i2_fus_ccda_open']) {
		assert.ok(en[k] && es[k] && en[k] !== es[k], k);
	}
	// an uploaded file is no download, and sections are no resources
	for (const k of ['i2_fus_ccda_d', 'i2_fus_ccda_open', 'i2_fus_ccda_narrative_t']) {
		assert.doesNotMatch(en[k], /download|resource/i, k);
		assert.doesNotMatch(es[k], /descargad|recurso/i, k);
	}
});

test('a C-CDA file refused for too many security labels says so in the person\'s language', () => {
	const has = (k: string) => k in en;
	const why = 'the file carries more than 64 different security labels; a C-CDA document needs far fewer';
	assert.equal(errKey(null, `not a readable C-CDA document: ${why}`, has), 'err_ccda_too_many_labels');
	assert.ok(en.err_ccda_too_many_labels && es.err_ccda_too_many_labels);
	assert.notEqual(en.err_ccda_too_many_labels, es.err_ccda_too_many_labels);
	assert.match(en.err_ccda_too_many_labels, /64/);
	assert.match(es.err_ccda_too_many_labels, /64/);
});
