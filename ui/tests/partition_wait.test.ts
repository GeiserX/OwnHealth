// An import that waits for the wearable samples table to be split by month (server slice 65): its
// job is queued again, and the person reads our sentence in each registry language, never the
// server's English.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { errKey } from '../src/lib/logic/errors.ts';
import { LANGS } from '../src/lib/i18n/langs.ts';
import { dict } from '../src/lib/i18n/dict.ts';

const has = (k: string) => k in dict.en;

test('the wait for the monthly split reads as our sentence', () => {
	const message =
		'PartitionPendingError: some samples are dated on or after the day the wearable samples are being split by month: the import runs again in an hour';
	assert.equal(errKey(null, message, has), 'err_wearable_split_wait');
});

test('the wait sentence exists in every registry language', () => {
	for (const l of LANGS) {
		const table = l.table as Record<string, string>;
		assert.ok(table.err_wearable_split_wait?.trim(), l.code);
	}
});
