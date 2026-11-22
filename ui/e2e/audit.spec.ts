// The audit card on MCP & access, as a person reads it in es and en: the log verified up to its last
// entry, the head the last backup kept outside the database, and what each tamper does to both. The
// database owner's edits run as SQL in the test database (serve.sh made it); the anchor file is the one
// `ownhealth backup` writes, put in OWNHEALTH_ANCHOR_DIR by hand. Every edit is undone at the end:
// the rows go back and the chain is hashed again, which gives the same hashes as before. Then the
// who-saw-my-data card: everyone on a profile Alex owns, only his own entries on one he cares for.
import { expect, test, type Page } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';

const db = process.env.OWNHEALTH_E2E_DB ?? '';
const anchorDir = process.env.OWNHEALTH_E2E_ANCHOR_DIR ?? '';

/** One statement (or a batch) in the test database; the first line of its answer. */
function sql(q: string): string {
	return execFileSync('docker', ['exec', 'ownhealth-testpg', 'psql', '-U', 'postgres', '-d', db, '-qAtc', q], { encoding: 'utf8' }).trim();
}

/** As the database owner: hash every link again from seq 1, holding the lock new links take. */
const REHASH = `do $$ declare c record; prev bytea := decode(repeat('00', 32), 'hex'); h bytea; begin
  perform pg_advisory_xact_lock(hashtext('ownhealth.audit_chain'));
  for c in select seq, event_id from audit_chain order by seq loop
    select sha256(prev || oh_audit_row_v1(e)) into h from audit_event e where e.id = c.event_id;
    update audit_chain set prev_hash = prev, hash = h where seq = c.seq;
    prev := h;
  end loop; end $$`;

function asOwner(statements: string) {
	sql(`alter table audit_event disable trigger user; alter table audit_chain disable trigger user; ${statements}; alter table audit_event enable trigger user; alter table audit_chain enable trigger user`);
}

function writeAnchor(seq: number, hash: string) {
	mkdirSync(anchorDir, { recursive: true });
	writeFileSync(`${anchorDir}/audit-head.json`, JSON.stringify({ schema: 'ownhealth.audit-anchor/v1', seq, hash, at: '2026-11-22T08:00:00+00:00' }));
}

function collectPageErrors(page: Page): string[] {
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	return errors;
}

test.describe.configure({ mode: 'serial' });

test('the audit card shows the log verified, anchored, rewritten and broken, in en and es', async ({ page }) => {
	const errors = collectPageErrors(page);
	expect(db, 'the config names the database').not.toBe('');
	expect(anchorDir, 'the config names the anchor folder').not.toBe('');
	const alex = sql("select id from profile where slug = 'alex'");
	sql(`insert into audit_event (actor_kind, via, action, tool, profile_id) values ('system', 'job', 'read', 'e2e', '${alex}'), ('system', 'job', 'read', 'e2e', '${alex}')`);
	const integrity = page.getByRole('group', { name: 'The log itself' });

	await page.goto('/mcp-access?lang=en');
	await expect(integrity.getByText(/Every entry is verified, up to entry \d+\./)).toBeVisible();
	await expect(integrity.getByText('No backup has kept a copy outside the database yet. The next backup does.')).toBeVisible();

	// a backup anchors the head: the page shows the entry and the day it was kept
	const [seq, hash] = sql('select seq, encode(hash, \'hex\') from audit_chain order by seq desc limit 1').split('|');
	writeAnchor(Number(seq), hash);
	await page.reload();
	await expect(integrity.getByText(`It matches entry ${seq} as the backup of 2026-11-22 kept it outside the database.`)).toBeVisible();

	// the owner edits the oldest entry and hashes the chain again: every link holds, the anchor does not
	const first = sql('select min(id) from audit_event');
	const tool = sql(`select coalesce(tool, '<null>') from audit_event where id = ${first}`);
	asOwner(`update audit_event set tool = 'rewritten' where id = ${first}; ${REHASH}`);
	await page.reload();
	await expect(integrity.getByText(/Every entry is verified, up to entry \d+\./)).toBeVisible();
	await expect(integrity.getByText(new RegExp(`It no longer matches entry ${seq} as the backup of 2026-11-22 kept it outside the database: someone with database rights rewrote or removed entries\\. Backups stop until the operator checks it\\.`))).toBeVisible();
	await expect(integrity.getByText('Problem', { exact: true })).toBeVisible();

	// an edit without the rehash: the chain itself names the entry
	const last = sql('select seq || \'|\' || event_id from audit_chain order by seq desc limit 1').split('|');
	const lastTool = sql(`select coalesce(tool, '<null>') from audit_event where id = ${last[1]}`);
	asOwner(`update audit_event set tool = 'edited' where id = ${last[1]}`);
	await page.goto('/mcp-access?lang=es');
	const integridad = page.getByRole('group', { name: 'El propio registro' });
	await expect(integridad.getByText(new RegExp(`La entrada ${last[0]} se cambió después de escribirse\\. Las entradas hasta la ${Number(last[0]) - 1} están verificadas\\.`))).toBeVisible();
	await expect(integridad.getByText(new RegExp(`Ya no coincide con la entrada ${seq} `))).toBeVisible();

	// undo: the rows as they were and the chain hashed again give back the anchored hash
	const back = (t: string) => (t === '<null>' ? 'null' : `'${t.replaceAll("'", "''")}'`);
	asOwner(`update audit_event set tool = ${back(tool)} where id = ${first}; update audit_event set tool = ${back(lastTool)} where id = ${last[1]}; ${REHASH}`);
	await page.reload();
	await expect(integridad.getByText(/Todas las entradas están verificadas, hasta la entrada \d+\./)).toBeVisible();
	await expect(integridad.getByText(`Coincide con la entrada ${seq} tal como la guardó fuera de la base de datos la copia de seguridad del 2026-11-22.`)).toBeVisible();
	expect(errors).toEqual([]);
});

test('who saw my data lists everyone on a profile Alex owns, and only his own entries on one he cares for', async ({ page }) => {
	const errors = collectPageErrors(page);
	const carmen = sql("select id from profile where slug = 'carmen'");
	sql(`insert into audit_event (actor_kind, via, action, tool, profile_id) values ('system', 'job', 'read', 'e2e', '${carmen}')`);

	await page.goto('/mcp-access?lang=en');
	const card = page.getByRole('region', { name: "Who saw Alex's data" });
	await expect(card).toBeVisible();
	const row = card.getByRole('row').filter({ hasText: 'OwnHealth itself' });
	await expect(row.getByRole('cell').first()).toHaveText('OwnHealth');
	await expect(row).toContainText('2'); // the two job reads the first check added on Alex

	await card.getByRole('combobox', { name: 'Profile' }).selectOption('carmen');
	const carmenCard = page.getByRole('region', { name: "Who saw Carmen's data" });
	await expect(carmenCard.getByText('You do not manage this profile, so you see only what you did on it.')).toBeVisible();
	await expect(carmenCard.getByText('OwnHealth itself')).toHaveCount(0); // the job read of Carmen is not his

	await page.goto('/mcp-access?lang=es');
	const tarjeta = page.getByRole('region', { name: 'Quién vio los datos de Alex' });
	await expect(tarjeta.getByRole('row').filter({ hasText: 'el propio OwnHealth' })).toBeVisible();
	await expect(tarjeta.getByRole('columnheader', { name: 'Entradas' })).toBeVisible();
	expect(errors).toEqual([]);
});
