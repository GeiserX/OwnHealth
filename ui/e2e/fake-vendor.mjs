// A fake SMART on FHIR vendor for the browser checks (connectors.md 6.3 rule 10): discovery, a
// sign-in page that consents at once and sends the browser back with a code, a token endpoint, RFC
// 7009 revocation, and a FHIR API with one patient and no resources for the SMART poller, on
// loopback over plain http. The server reaches it because serve.sh runs the debug binary with
// OWNHEALTH_DEV_CONNECTOR_LOOPBACK=1. Every token is synthetic.
//
// The first grant lasts 3 days, so the Connections screen shows a sign-in that ends soon; every later
// grant lasts 90 days. GET /__book says what the fake saw.
import { createHash, randomUUID } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';

const cms = (kind) => JSON.parse(readFileSync(new URL(`../../fixtures/bluebutton/cms-${kind}.json`, import.meta.url)));
const cmsPatient = { ...cms('patient'), name: [{ family: 'Rivera', given: ['Alex'] }], birthDate: '1979-05-14' };
const cmsRows = { Patient: cmsPatient, Coverage: cms('coverage'), ExplanationOfBenefit: cms('eob') };
const cmsScope = 'launch/patient patient/Patient.rs patient/Coverage.rs patient/ExplanationOfBenefit.rs';
const cmsCodes = new Map();
const cmsAccess = new Set();


const port = Number(process.env.OWNHEALTH_E2E_FAKE_PORT ?? 9190);
const base = `http://127.0.0.1:${port}`;
const book = { grants: 0, revoked: 0, refused: 0, fhir: 0 };
let n = 0;
const owOAuth = '/api/v1/oauth';
const owStates = new Map();
const owCallback = new URL('/ow/api/v1/oauth/whoop/callback', `http://localhost:${process.env.OWNHEALTH_E2E_PORT ?? 8190}`);

function json(res, status, body) {
	res.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' });
	res.end(JSON.stringify(body));
}

function form(req) {
	return new Promise((resolve) => {
		let raw = '';
		req.on('data', (c) => (raw += c));
		req.on('end', () => resolve(new URLSearchParams(raw)));
	});
}

createServer(async (req, res) => {
	const url = new URL(req.url ?? '/', base);
	// Fake both sides of the WHOOP flow: OW issues the state, the vendor returns a code, and
	// only the OW callback can redeem it once. OW redirects to its unprefixed result path.
	if (req.method === 'GET' && url.pathname === `${owOAuth}/whoop/authorize`) {
		const user = url.searchParams.get('user_id');
		if (!user) return json(res, 400, { error: 'missing_user' });
		const state = randomUUID();
		owStates.set(state, { user, code: null });
		const auth = new URL('/whoop/consent', base);
		auth.searchParams.set('redirect_uri', owCallback.href);
		auth.searchParams.set('state', state);
		return json(res, 200, { authorization_url: auth.href, state });
	}
	if (req.method === 'GET' && url.pathname === '/whoop/consent') {
		const state = url.searchParams.get('state');
		const flow = owStates.get(state);
		if (url.searchParams.get('redirect_uri') !== owCallback.href || !flow?.user) {
			return json(res, 400, { error: 'invalid_authorization' });
		}
		flow.code = randomUUID();
		const back = new URL(owCallback);
		back.searchParams.set('state', state);
		back.searchParams.set('code', flow.code);
		res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
		return res.end(`<title>WHOOP consent</title><a href="${back.href.replaceAll('&', '&amp;')}">Allow WHOOP access</a>`);
	}
	if (req.method === 'GET' && url.pathname === `${owOAuth}/whoop/callback`) {
		const state = url.searchParams.get('state');
		const flow = owStates.get(state);
		let location = `${owOAuth}/error?message=Invalid+OAuth+state+or+code`;
		if (flow?.code && flow.code === url.searchParams.get('code')) {
			owStates.delete(state);
			location = `${owOAuth}/success?${new URLSearchParams({ provider: 'whoop', user_id: flow.user })}`;
		}
		res.writeHead(303, { location });
		return res.end();
	}
	if (req.method === 'GET' && url.pathname === `${owOAuth}/success`) {
		return json(res, 200, { success: true, provider: url.searchParams.get('provider'), user_id: url.searchParams.get('user_id') });
	}
	if (req.method === 'GET' && url.pathname === `${owOAuth}/error`) {
		return json(res, 200, { success: false, message: url.searchParams.get('message') });
	}
	if (req.method === 'GET' && url.pathname === '/.well-known/smart-configuration') {
		return json(res, 200, {
			issuer: base,
			authorization_endpoint: `${base}/authorize`,
			token_endpoint: `${base}/token`,
			revocation_endpoint: `${base}/revoke`,
			code_challenge_methods_supported: ['S256'],
			token_endpoint_auth_methods_supported: ['client_secret_basic', 'client_secret_post'],
			capabilities: ['launch-standalone', 'client-confidential-symmetric', 'permission-offline', 'permission-v2']
		});
	}
	if (req.method === 'GET' && url.pathname === '/v3/o/authorize/') {
		const q = url.searchParams;
		const state = q.get('state') ?? '';
		const challenge = q.get('code_challenge');
		if (state.length < 16 || !challenge || q.get('code_challenge_method') !== 'S256' || q.get('scope') !== cmsScope) {
			return json(res, 400, { error: 'invalid_request' });
		}
		const code = randomUUID();
		cmsCodes.set(code, challenge);
		const back = new URL(q.get('redirect_uri') ?? '');
		back.searchParams.set('state', state);
		back.searchParams.set('code', code);
		res.writeHead(302, { location: back.toString() });
		return res.end();
	}
	if (req.method === 'POST' && url.pathname === '/v3/o/token/') {
		const f = await form(req);
		const code = f.get('code');
		const challenge = createHash('sha256').update(f.get('code_verifier') ?? '').digest('base64url');
		const basic = (req.headers.authorization ?? '').split(' ');
		if (basic[0] !== 'Basic' || !Buffer.from(basic[1] ?? '', 'base64').toString().startsWith('e2e-medicare:')
			|| f.get('grant_type') !== 'authorization_code' || cmsCodes.get(code) !== challenge) {
			return json(res, 400, { error: 'invalid_grant' });
		}
		cmsCodes.delete(code);
		const access = randomUUID();
		cmsAccess.add(access);
		return json(res, 200, { access_token: access, refresh_token: randomUUID(), token_type: 'Bearer',
			expires_in: 3600, scope: cmsScope, patient: cmsPatient.id });
	}
	const fhir = /^\/v3\/fhir\/(Patient|Coverage|ExplanationOfBenefit)\/$/.exec(url.pathname);
	if (req.method === 'GET' && fhir) {
		if (!cmsAccess.has((req.headers.authorization ?? '').replace(/^Bearer /, ''))) {
			return json(res, 401, { error: 'invalid_token' });
		}
		const q = url.searchParams;
		const bound = fhir[1] === 'Patient' ? q.get('_id') === cmsPatient.id
			: fhir[1] === 'Coverage' ? q.get('beneficiary') === `Patient/${cmsPatient.id}` : q.get('patient') === cmsPatient.id;
		if (!bound || q.get('_count') !== '50' || q.get('_offset') !== '0') return json(res, 400, { error: 'invalid_query' });
		return json(res, 200, { resourceType: 'Bundle', type: 'searchset', total: 1, entry: [{ resource: cmsRows[fhir[1]] }] });
	}
	if (req.method === 'GET' && url.pathname === '/authorize') {
		const back = new URL(url.searchParams.get('redirect_uri') ?? '');
		back.searchParams.set('state', url.searchParams.get('state') ?? '');
		back.searchParams.set('code', `fake-code-${++n}`);
		res.writeHead(302, { location: back.toString() });
		return res.end();
	}
	if (req.method === 'POST' && url.pathname === '/token') {
		const f = await form(req);
		if (f.get('grant_type') !== 'authorization_code' || !f.get('code_verifier')) {
			book.refused++;
			return json(res, 400, { error: 'invalid_grant' });
		}
		book.grants++;
		return json(res, 200, {
			access_token: `fake-access-${book.grants}`,
			token_type: 'Bearer',
			expires_in: 3600,
			refresh_token: `fake-refresh-${book.grants}`,
			refresh_token_expires_in: (book.grants === 1 ? 3 : 90) * 86_400,
			scope: 'openid fhirUser launch/patient offline_access patient/*.rs',
			patient: 'fake-patient-alex-rivera'
		});
	}
	if (req.method === 'POST' && url.pathname === '/revoke') {
		await form(req);
		book.revoked++;
		res.writeHead(200);
		return res.end();
	}
	// The FHIR API the SMART poller reads once a connection is made: the patient, and no resources.
	if (req.method === 'GET' && /^\/Patient\/[A-Za-z0-9.-]+$/.test(url.pathname)) {
		book.fhir++;
		return json(res, 200, {
			resourceType: 'Patient',
			id: 'fake-patient-alex-rivera',
			name: [{ family: 'Rivera', given: ['Alex'] }],
			birthDate: '1979-05-14'
		});
	}
	if (req.method === 'GET' && /^\/[A-Z][A-Za-z]+$/.test(url.pathname)) {
		book.fhir++;
		return json(res, 200, { resourceType: 'Bundle', type: 'searchset', total: 0, entry: [] });
	}
	if (req.method === 'GET' && url.pathname === '/__book') return json(res, 200, book);
	json(res, 404, { error: 'not_found' });
}).listen(port, '127.0.0.1');
