// The add-connection wizard (slice 19): which saved client a server uses, what the save and the launch
// send, the token facts it shows per vendor, and words in es and en for every code the save answers.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dict } from '../src/lib/i18n/dict.ts';
import {
	bare,
	clientFor,
	defaultLabel,
	hitsOf,
	needsSecret,
	saveIssuer,
	scopeLine,
	serverPick,
	tokenFacts,
	typedServer,
	type SavedClient,
	type VendorProfile
} from '../src/routes/connections/add/wizard.ts';

const vendor = (over: Partial<VendorProfile> = {}): VendorProfile => ({
	key: 'healow',
	name: 'eClinicalWorks / healow',
	kind: 'smart',
	client_type: 'confidential',
	servers: [],
	directory: true,
	registration: 'https://connect4.healow.com/',
	scopes: ['openid', 'fhirUser', 'launch/patient', 'offline_access', 'patient/Patient.rs'],
	refresh: { rotation: 'rotating', unused_expiry_days: null },
	access_token_seconds: null,
	verified_live: false,
	sources: [],
	...over
});

const saved = (over: Partial<SavedClient> = {}): SavedClient => ({
	id: 'c1',
	vendor: 'healow',
	issuer: 'https://fhir4.healow.com/fhir/r4/LABNORTE',
	client_id: 'rivera-app',
	confidential: true,
	redirect_uri: 'https://ownhealth.rivera.example/api/connectors/callback',
	created_at: '2026-11-25T10:00:00Z',
	...over
});

test('a confidential client serves only the server it was saved for', () => {
	const v = vendor();
	const c = saved();
	assert.equal(clientFor([c], v, 'https://fhir4.healow.com/fhir/r4/LABNORTE/'), c);
	assert.equal(clientFor([c], v, 'https://fhir4.healow.com/fhir/r4/OTHER'), null);
	// another vendor's client for the same address is never used
	assert.equal(clientFor([saved({ vendor: 'epic' })], v, c.issuer!), null);
	// so the save names the server, and a confidential vendor asks for the secret
	assert.equal(saveIssuer(v, ' https://fhir4.healow.com/fhir/r4/LABNORTE/ '), 'https://fhir4.healow.com/fhir/r4/LABNORTE');
	assert.equal(needsSecret(v), true);
});

test('a public client saved without a server serves every server of its vendor', () => {
	const v = vendor({ key: 'oracle', client_type: 'public' });
	const any = saved({ vendor: 'oracle', issuer: null, confidential: false });
	assert.equal(clientFor([any], v, 'https://fhir-myrecord.cerner.com/r4/tenant-a'), any);
	// one saved for a named server wins over the one for every server
	const named = saved({ id: 'c2', vendor: 'oracle', issuer: 'https://fhir-myrecord.cerner.com/r4/tenant-a', confidential: false });
	assert.equal(clientFor([any, named], v, 'https://fhir-myrecord.cerner.com/r4/tenant-a'), named);
	assert.equal(saveIssuer(v, 'https://fhir-myrecord.cerner.com/r4/tenant-a'), undefined);
	assert.equal(needsSecret(v), false);
	// a confidential vendor never falls back to a client with no server
	assert.equal(clientFor([saved({ issuer: null })], vendor(), 'https://fhir4.healow.com/fhir/r4/X'), null);
});

test('the server is searched in the directory or picked from the vendor list', () => {
	assert.equal(serverPick(vendor()), 'directory');
	assert.equal(serverPick(vendor({ directory: false, servers: ['https://api.va.gov/services/fhir/v0/r4'] })), 'list');
	const hits = [
		{ name: 'LabNorte Clinic', endpoint: 'https://a.example/r4', state: 'TX', vendor: 'healow' },
		{ name: 'LabNorte Hospital', endpoint: 'https://b.example/r4', state: 'TX', vendor: 'epic' }
	];
	assert.deepEqual(hitsOf(hits, vendor()).map((h) => h.vendor), ['healow']);
});

test('a typed server address counts once it is a web address', () => {
	assert.equal(typedServer(' https://fhir.labnorte.example/r4/ '), 'https://fhir.labnorte.example/r4');
	assert.equal(typedServer('http://127.0.0.1:9190/v3/fhir'), 'http://127.0.0.1:9190/v3/fhir');
	assert.equal(typedServer('fhir.labnorte.example'), null);
	assert.equal(typedServer('ftp://fhir.labnorte.example'), null);
	assert.equal(typedServer(''), null);
	assert.equal(bare('https://x.example///'), 'https://x.example');
});

test('a new connection is named after the facility, else the server host', () => {
	assert.equal(defaultLabel('https://fhir.labnorte.example/r4', ' LabNorte Clinic '), 'LabNorte Clinic');
	assert.equal(defaultLabel('https://fhir.labnorte.example/r4', null), 'fhir.labnorte.example');
	assert.equal(defaultLabel('not a url', ''), '');
});

test('the scopes paste as one line, and the token facts follow the profile', () => {
	assert.equal(scopeLine(vendor()), 'openid fhirUser launch/patient offline_access patient/Patient.rs');
	const va = vendor({ refresh: { rotation: 'unknown', unused_expiry_days: 42 } });
	assert.deepEqual(tokenFacts(va), [
		{ key: 'cw_rot_unknown', vars: {} },
		{ key: 'cw_unused_days', vars: { n: 42 } }
	]);
	const oracle = vendor({ refresh: { rotation: 'none', unused_expiry_days: 90 }, access_token_seconds: 570 });
	assert.deepEqual(tokenFacts(oracle).map((f) => f.key), ['cw_rot_none', 'cw_unused_days', 'cw_access_sec']);
	const minutes = vendor({ refresh: { rotation: 'single_use', unused_expiry_days: null }, access_token_seconds: 3600 });
	assert.deepEqual(tokenFacts(minutes), [
		{ key: 'cw_rot_single_use', vars: {} },
		{ key: 'cw_unused_unknown', vars: {} },
		{ key: 'cw_access_min', vars: { n: 60 } }
	]);
	// every fact the wizard can show has words in es and en
	const en = dict.en as Record<string, string>;
	const es = dict.es as Record<string, string>;
	const keys = ['none', 'rotating', 'single_use', 'unknown'].map((r) => 'cw_rot_' + r).concat(['cw_unused_days', 'cw_unused_unknown', 'cw_access_min', 'cw_access_sec']);
	assert.deepEqual(keys.filter((k) => !en[k] || !es[k] || en[k] === es[k]), []);
});

// Every code the client save answers with is read from the Rust source, so a new code without words
// here fails this test.
const routes = readFileSync(new URL('../../server/crates/ownhealth-api/src/routes/connectors.rs', import.meta.url), 'utf8');

test('every code of the client save reads in es and en', () => {
	const codes = new Set([...routes.matchAll(/unfit\(\s*"([a-z_]+)"/g)].map((m) => m[1]));
	// the scan can fail: the codes the save route has today are all found
	for (const c of ['unknown_vendor', 'bad_client_id', 'bad_client_secret', 'client_secret_missing', 'client_secret_unexpected', 'issuer_required', 'bad_issuer']) {
		assert.ok(codes.has(c), `the scan missed ${c}`);
	}
	const en = dict.en as Record<string, string>;
	const es = dict.es as Record<string, string>;
	assert.deepEqual([...codes].filter((c) => !en['con_err_' + c] || !es['con_err_' + c] || en['con_err_' + c] === es['con_err_' + c]), []);
});
