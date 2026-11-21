// The line after a profile delete: when some files could not be removed yet, it says so in both
// languages instead of saying the files are gone.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { deletedNoteKey } from '../src/lib/logic/people.ts';

test('a delete with files still pending names them', () => {
	assert.equal(deletedNoteKey(0), 'st_deleted');
	assert.equal(deletedNoteKey(3), 'st_deleted_pending');
	const dict = readFileSync(new URL('../src/lib/i18n/dict.ts', import.meta.url), 'utf8');
	const lines = dict.split('\n').filter((l) => l.trim().startsWith('st_deleted_pending:'));
	assert.equal(lines.length, 2, 'one line per language');
	for (const l of lines) assert.ok(l.includes('{name}') && l.includes('{n}'), l);
	const page = readFileSync(new URL('../src/routes/settings/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /blobs_pending/);
	assert.match(page, /tf\(deletedNoteKey\(deletedPending\)/);
});
