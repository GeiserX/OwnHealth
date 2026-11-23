// The line after a profile delete: when some files could not be removed yet, it says so in both
// languages instead of saying the files are gone.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { deletedNoteKey } from '../src/lib/logic/people.ts';
import { errKey } from '../src/lib/logic/errors.ts';
import { dict } from '../src/lib/i18n/dict.ts';

test('a delete with files still pending names them', () => {
	assert.equal(deletedNoteKey(0, 0), 'st_deleted');
	assert.equal(deletedNoteKey(3, 0), 'st_deleted_pending');
	assert.equal(deletedNoteKey(0, 1), 'st_deleted_pending');
	const dict = readFileSync(new URL('../src/lib/i18n/dict.ts', import.meta.url), 'utf8');
	const lines = dict.split('\n').filter((l) => l.trim().startsWith('st_deleted_pending:'));
	assert.equal(lines.length, 2, 'one line per language');
	for (const l of lines) assert.ok(l.includes('{name}') && !l.includes('{n}'), l);
	const page = readFileSync(new URL('../src/routes/settings/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /blobs_pending/);
	assert.match(page, /folders_pending/);
});

// The delete could not rewrite keyring.json: older backups still open the profile's files until it
// does, so the line says the key is still there instead of saying the files are gone.
test('a delete whose keyring rewrite is pending says the key is still in keyring.json', () => {
	assert.equal(deletedNoteKey(0, 0, true), 'st_deleted_keyring');
	assert.equal(deletedNoteKey(3, 1, true), 'st_deleted_keyring');
	assert.equal(deletedNoteKey(0, 0, false), 'st_deleted');
	for (const lang of ['en', 'es'] as const) {
		const line = (dict[lang] as Record<string, string>).st_deleted_keyring;
		assert.ok(line && line.includes('{name}') && line.includes('keyring.json'), `${lang}: ${line}`);
	}
	const page = readFileSync(new URL('../src/routes/settings/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /keyring_pending/);
});

// The delete's retry gave up: the review queue says what is left in the reader's language, never the
// server's English and never "Import failed".
test('a delete retry that gave up reads as our sentence', () => {
	const has = (k: string) => k in dict.en;
	assert.equal(errKey(null, '1 files, folders or keyring entries could not be removed yet', has), 'err_delete_cleanup');
	for (const lang of ['en', 'es'] as const) {
		const d = dict[lang] as Record<string, string>;
		assert.ok(d.err_delete_cleanup && d.i2_jf_unlink_t, lang);
	}
	const page = readFileSync(new URL('../src/routes/import-review/+page.svelte', import.meta.url), 'utf8');
	assert.match(page, /blobs\.unlink/);
});

// An upload to a profile a restore brought back after its delete answers 410 `profile_deleted`.
test('an upload to a deleted profile reads as our sentence', () => {
	const has = (k: string) => k in dict.en;
	assert.equal(errKey('profile_deleted', 'This profile was deleted.', has), 'err_profile_deleted');
	for (const lang of ['en', 'es'] as const) assert.ok((dict[lang] as Record<string, string>).err_profile_deleted, lang);
});
