// A fake Orthanc for the imaging browser checks (imaging.spec.ts), on loopback over plain http. It
// answers the two calls the app makes to open a study: POST /tools/lookup (which Orthanc study holds a
// StudyInstanceUID) and GET /ohif/<file> (the viewer's files, from Orthanc's OHIF plugin), and only
// with the basic auth the app sends (user ownhealth, the password serve.sh gives the app). A UID under
// FAKE_ROOT is held as the one study `fake-<uid>`, the Orthanc id the checks seed on the record; any
// other UID is unknown. The viewer is a stub page that says which file it is. GET /healthz needs no
// auth, so Playwright can see it is up.
import { createServer } from 'node:http';

const port = Number(process.env.OWNHEALTH_E2E_ORTHANC_PORT ?? 10190);
const password = process.env.OWNHEALTH_E2E_ORTHANC_PASSWORD ?? '';
const auth = 'Basic ' + Buffer.from(`ownhealth:${password}`).toString('base64');
const FAKE_ROOT = '1.2.826.0.1.3680043.10.4242.';

function send(res, status, type, body) {
	res.writeHead(status, { 'content-type': type, 'cache-control': 'no-store' });
	res.end(body);
}

function text(req) {
	return new Promise((resolve) => {
		let raw = '';
		req.on('data', (c) => (raw += c));
		req.on('end', () => resolve(raw));
	});
}

createServer(async (req, res) => {
	const url = new URL(req.url ?? '/', `http://127.0.0.1:${port}`);
	if (req.method === 'GET' && url.pathname === '/healthz') return send(res, 200, 'text/plain', 'ok');
	if (!password || req.headers.authorization !== auth) {
		res.writeHead(401, { 'www-authenticate': 'Basic realm="Orthanc"' });
		return res.end();
	}
	if (req.method === 'POST' && url.pathname === '/tools/lookup') {
		const uid = (await text(req)).trim();
		const found = uid.startsWith(FAKE_ROOT) ? [{ ID: `fake-${uid}`, Path: `/studies/fake-${uid}`, Type: 'Study' }] : [];
		return send(res, 200, 'application/json', JSON.stringify(found));
	}
	if (req.method === 'GET' && url.pathname.startsWith('/ohif/')) {
		const file = url.pathname.slice('/ohif/'.length);
		return send(res, 200, 'text/html; charset=utf-8', `<!doctype html><title>OHIF (fake)</title><h1 id="fake-ohif">Fake OHIF: ${file}</h1>`);
	}
	send(res, 404, 'text/plain', 'not found');
}).listen(port, '127.0.0.1');
