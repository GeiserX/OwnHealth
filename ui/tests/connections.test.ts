// The Connections screen (slice 55): every connection with its status, last sync, token ends and a
// Reconnect button, and the words for every code the connector callback sends back.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dict } from '../src/lib/i18n/dict.ts';
import {
	GRANT_SOON_DAYS,
	callbackOutcome,
	canReconnect,
	connectionState,
	connectorErrorKey,
	daysLeft,
	shownConnections,
	statusWord,
	wantsReconnect,
	type ConnectionRow
} from '../src/lib/logic/connections.ts';

const NOW = new Date('2026-11-22T10:00:00Z');
const inDays = (d: number) => new Date(NOW.getTime() + d * 86_400_000).toISOString();

const conn = (over: Partial<ConnectionRow> = {}): ConnectionRow => ({
	id: 'c1',
	kind: 'smart',
	label: 'Hospital LabNorte',
	profile_id: 'p-alex',
	enabled: true,
	last_run_at: inDays(-0.1),
	last_success_at: inDays(-0.1),
	last_error_at: null,
	last_error: null,
	consecutive_failures: 0,
	stale: false,
	last_run_status: 'ok',
	last_run_problems: 0,
	last_run_problem: null,
	reconsent_needed_at: null,
	grant_expires_at: inDays(90),
	access_expires_at: inDays(0.04),
	open_gaps: 0,
	server: 'https://fhir.labnorte.example/r4',
	can_manage: true,
	...over
});

test('a connection the vendor no longer accepts shows the Reconnect button, and a reconnect clears it', () => {
	const flagged = conn({ reconsent_needed_at: inDays(-1) });
	assert.equal(connectionState(flagged, NOW), 'reconsent');
	assert.equal(statusWord('reconsent'), 'out');
	assert.equal(wantsReconnect('reconsent'), true);
	assert.equal(canReconnect(flagged), true);
	// the reverse path: the callback cleared reconsent_needed_at, so the row reads OK again
	const back = conn({ reconsent_needed_at: null });
	assert.equal(connectionState(back, NOW), 'ok');
	assert.equal(wantsReconnect(connectionState(back, NOW)), false);
});

test('a grant that ends soon shows before it ends, and one that ended asks for a reconnect', () => {
	assert.equal(GRANT_SOON_DAYS, 7);
	const soon = conn({ grant_expires_at: inDays(3) });
	assert.equal(connectionState(soon, NOW), 'grant_soon');
	assert.equal(statusWord('grant_soon'), 'watch');
	assert.equal(wantsReconnect('grant_soon'), true);
	assert.equal(daysLeft(soon.grant_expires_at, NOW), 3);
	assert.equal(connectionState(conn({ grant_expires_at: inDays(GRANT_SOON_DAYS + 1) }), NOW), 'ok');
	assert.equal(connectionState(conn({ grant_expires_at: inDays(-0.5) }), NOW), 'expired');
	assert.equal(statusWord('expired'), 'out');
	// no grant end known: nothing to warn about
	assert.equal(connectionState(conn({ grant_expires_at: null }), NOW), 'ok');
	// reconsent wins over a short grant
	assert.equal(connectionState(conn({ grant_expires_at: inDays(2), reconsent_needed_at: inDays(-0.1) }), NOW), 'reconsent');
});

test('the run states read as the five status words', () => {
	assert.equal(connectionState(conn({ enabled: false }), NOW), 'off');
	assert.equal(statusWord('off'), 'nodata');
	assert.equal(connectionState(conn({ consecutive_failures: 2, last_run_status: 'error', last_success_at: inDays(-2), last_error_at: inDays(-0.1), last_error: 'vendor_unreachable' }), NOW), 'out');
	assert.equal(connectionState(conn({ last_run_status: 'partial', last_run_problems: 1 }), NOW), 'partial');
	assert.equal(statusWord('partial'), 'watch');
	assert.equal(connectionState(conn({ stale: true }), NOW), 'stale');
	assert.equal(connectionState(conn({ last_run_at: null, last_success_at: null, last_run_status: null }), NOW), 'nodata');
});

test('the screen lists only connections of profiles the person manages, and Reconnect only where the vendor signs in', () => {
	const rows = [
		conn({ id: 'mine' }),
		conn({ id: 'viewed', can_manage: false }),
		conn({ id: 'inbox', kind: 'email' }),
		conn({ id: 'ow', kind: 'openwearables', grant_expires_at: null }),
		conn({ id: 'token', can_manage: undefined })
	];
	assert.deepEqual(
		shownConnections(rows).map((r) => r.id),
		['mine', 'ow']
	);
	assert.equal(canReconnect(conn({ kind: 'dexcom' })), true);
	assert.equal(canReconnect(conn({ kind: 'bluebutton' })), true);
	assert.equal(canReconnect(conn({ kind: 'openwearables' })), false);
	assert.equal(canReconnect(conn({ can_manage: false })), false);
});

test('the callback query becomes one outcome: connected, or a fixed error code', () => {
	assert.deepEqual(callbackOutcome(new URLSearchParams('connected=c1')), { connected: 'c1' });
	assert.deepEqual(callbackOutcome(new URLSearchParams('connector_error=denied')), { error: 'denied' });
	assert.equal(callbackOutcome(new URLSearchParams('')), null);
	// a code is one of our short words; anything else is not echoed onto the screen
	assert.deepEqual(callbackOutcome(new URLSearchParams('connector_error=<b>hi</b>')), { error: 'unknown' });
	const has = (k: string) => k in dict.en;
	assert.equal(connectorErrorKey('denied', has), 'con_err_denied');
	assert.equal(connectorErrorKey('no_such_code', has), 'con_err_unknown');
});

// Every code the server can send back to /connections, or answer a reconnect or disconnect with, is
// read from the Rust source, so a new code without words here fails this test.
const read = (p: string) => readFileSync(new URL(p, import.meta.url), 'utf8');
const routes = read('../../server/crates/ownhealth-api/src/routes/connectors.rs');
const core = read('../../server/crates/ownhealth-core/src/connector_oauth.rs');
const codes = new Set<string>();
for (const m of routes.matchAll(/back\(&state, "([a-z_]+)"\)/g)) codes.add(m[1]);
for (const m of routes.matchAll(/ApiError::new\(\s*StatusCode::\w+,\s*"([a-z_]+)"/g)) codes.add(m[1]);
for (const m of core.matchAll(/refused\(\s*"([a-z_]+)"/g)) codes.add(m[1]);

test('every connector code reads in es and en', () => {
	// the gate can fail: the callback codes PR 69 fixed are all found
	for (const c of ['state_invalid', 'wrong_browser', 'denied', 'wrong_user', 'forbidden', 'client_gone', 'exchange_failed', 'wrong_household', 'source_changed', 'another_patient', 'reconsent_needed', 'vendor_unreachable']) {
		assert.ok(codes.has(c), `the scan missed ${c}`);
	}
	const en = dict.en as Record<string, string>;
	const es = dict.es as Record<string, string>;
	const missing = [...codes, 'unknown'].filter((c) => !en['con_err_' + c] || !es['con_err_' + c] || en['con_err_' + c] === es['con_err_' + c]);
	assert.deepEqual(missing, []);
});
