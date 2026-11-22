// A fake SMART on FHIR vendor for the browser checks (connectors.md 6.3 rule 10): discovery, a
// sign-in page that consents at once and sends the browser back with a code, a token endpoint and
// RFC 7009 revocation, on loopback over plain http. The server reaches it because serve.sh runs the
// debug binary with OWNHEALTH_DEV_CONNECTOR_LOOPBACK=1. Every token is synthetic.
//
// The first grant lasts 3 days, so the Connections screen shows a sign-in that ends soon; every later
// grant lasts 90 days. GET /__book says what the fake saw.
import { createServer } from 'node:http';

const port = Number(process.env.OWNHEALTH_E2E_FAKE_PORT ?? 9190);
const base = `http://127.0.0.1:${port}`;
const book = { grants: 0, revoked: 0, refused: 0 };
let n = 0;

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
	if (req.method === 'GET' && url.pathname === '/.well-known/smart-configuration') {
		return json(res, 200, {
			issuer: base,
			authorization_endpoint: `${base}/authorize`,
			token_endpoint: `${base}/token`,
			revocation_endpoint: `${base}/revoke`,
			code_challenge_methods_supported: ['S256'],
			token_endpoint_auth_methods_supported: ['client_secret_basic', 'client_secret_post']
		});
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
	if (req.method === 'GET' && url.pathname === '/__book') return json(res, 200, book);
	json(res, 404, { error: 'not_found' });
}).listen(port, '127.0.0.1');
