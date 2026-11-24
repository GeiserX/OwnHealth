// The C-CDA import (slice 20) on the review page: a refused file and the item that lists the sections
// holding text only read in the person's language, never in the server's English.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { CCDA_NARRATIVE, unmappedSummary } from '../src/lib/logic/inbox.ts';
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

test('the item for the sections that hold text only names them with a key, other types stay as sent', () => {
	const item = (resource_type: string) => ({
		kind: 'fhir_unmapped_summary',
		payload: { resource_type, count: 7, source: { kind: 'ccda', label: 'record.xml' } }
	});
	assert.equal(CCDA_NARRATIVE, 'C-CDA narrative section');
	assert.deepEqual(unmappedSummary(item(CCDA_NARRATIVE)), { type: CCDA_NARRATIVE, count: 7, source: 'record.xml', key: 'i2_fus_ccda_narrative' });
	// a section the document titles itself keeps the document's own words
	assert.deepEqual(unmappedSummary(item('PROCEDURES (C-CDA)')), { type: 'PROCEDURES (C-CDA)', count: 7, source: 'record.xml' });
	assert.ok(en.i2_fus_ccda_narrative && es.i2_fus_ccda_narrative && en.i2_fus_ccda_narrative !== es.i2_fus_ccda_narrative);
});
