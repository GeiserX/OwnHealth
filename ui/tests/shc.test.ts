// SMART Health Card receive (slice 22): every refusal reason and detail code has a sentence in both
// languages, a card for someone else says whose it is, and the receiver never calls a signer trusted
// or verified.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
	SHC_DETAILS,
	SHC_REASONS,
	SHL_NOT_IMPORTED,
	SHL_REASONS,
	declinedKey,
	detailKey,
	identityWarning,
	isLink,
	linkRefusalKey,
	linkRefusalVars,
	linkWarning,
	notImportedKey,
	passcodeRetry,
	receivedBefore,
	refusalKey,
	refusalVars,
	shcCards
} from '../src/lib/logic/shc.ts';
import { dict } from '../src/lib/i18n/dict.ts';

const tables = dict as unknown as Record<string, Record<string, string>>;
const fill = (s: string, vars: Record<string, string | number>) => s.replace(/\{(\w+)\}/g, (_, k) => String(vars[k] ?? `{${k}}`));
const words = (lang: string) => (key: string) => tables[lang][key] ?? key;

test('every refusal reason has an es and an en sentence, and an unknown one gets the general one', () => {
	for (const lang of ['en', 'es']) {
		for (const r of SHC_REASONS) assert.ok(tables[lang][refusalKey(r)], `${lang} ${r}`);
		assert.equal(refusalKey('something_new'), 'shc_r_other');
		assert.ok(tables[lang].shc_r_other);
	}
	assert.equal(refusalKey('bad_signature'), 'shc_r_bad_signature');
});

test('every detail code is worded in es and en, so no server text reaches the sentence', () => {
	for (const lang of ['en', 'es']) {
		for (const d of SHC_DETAILS) assert.ok(tables[lang][detailKey(d) ?? ''], `${lang} ${d}`);
		const http = refusalVars('keys', 'http_404', 'https://issuer.labnorte.example', words(lang));
		assert.match(String(http.detail), /404/, lang);
		const dns = refusalVars('revocation_list', 'dns', 'https://issuer.labnorte.example', words(lang));
		assert.equal(dns.detail, tables[lang].shc_d_dns, lang);
		// a code the UI does not know never shows raw
		assert.equal(refusalVars('keys', 'issuer.example does not resolve', null, words(lang)).detail, tables[lang].shc_d_other);
	}
	assert.equal(detailKey('http_503'), 'shc_d_http');
	assert.equal(detailKey('timeout'), 'shc_d_timeout');
});

test('a photo with only some codes of a split card says how many could be read', () => {
	const v = refusalVars('chunks_unread', '2/3', null, words('en'));
	assert.equal(fill(tables.en.shc_r_chunks_unread, v), '2 of 3 codes could be read. Take a closer photo of the whole sheet.');
	assert.equal(fill(tables.es.shc_r_chunks_unread, refusalVars('chunks_unread', '2/3', null, words('es'))), 'Se pudieron leer 2 de 3 códigos. Haz una foto más cercana de toda la hoja.');
});

test('a missing code of a split card is named by its number', () => {
	assert.equal(fill(tables.en.shc_r_chunk_missing, refusalVars('chunk_missing', '2/3', null, words('en'))), 'This card is split over several QR codes, and code 2 is missing.');
	assert.equal(fill(tables.es.shc_r_chunk_missing, refusalVars('chunk_missing', '2/3', null, words('es'))), 'Esta tarjeta está repartida en varios códigos QR y falta el código 2.');
});

test('a card for someone else says whose it is, before anything is accepted', () => {
	const carmen = { name: 'Carmen Ortega', birth_date: '1955-01-30' };
	assert.equal(identityWarning('matched', [carmen], 'Alex Rivera'), null);
	const w = identityWarning('mismatch', [carmen], 'Alex Rivera');
	assert.ok(w);
	assert.equal(fill(tables.en[w.key], w.vars), 'This card names Carmen Ortega, born 1955-01-30, not Alex Rivera. Its rows wait in review.');
	assert.equal(fill(tables.es[w.key], w.vars), 'Esta tarjeta es de Carmen Ortega (fecha de nacimiento: 1955-01-30), no de Alex Rivera. Sus filas esperan en revisión.');
	for (const id of ['missing', 'hinted', 'mismatch']) {
		const x = identityWarning(id, [{ name: null, birth_date: null }], 'Alex Rivera');
		assert.ok(x && tables.en[x.key] && tables.es[x.key], id);
	}
	// in a file of several cards the server gives one result for all, so no card is named
	const alex = { name: 'Alex Rivera', birth_date: '1979-05-14' };
	const batch = identityWarning('mismatch', [alex, carmen], 'Alex Rivera');
	assert.equal(batch && fill(tables.en[batch.key], batch.vars), 'One of these cards may be for someone other than Alex Rivera. Its rows wait in review.');
	assert.equal(batch && fill(tables.es[batch.key], batch.vars), 'Alguna de estas tarjetas puede ser de otra persona, no de Alex Rivera. Sus filas esperan en revisión.');
});

test('a card document lists its cards from the document flags, and any other document none', () => {
	const flags = {
		shc: [{ iss: 'https://issuer.labnorte.example', host: 'issuer.labnorte.example', kid: 'k', issued: '2026-10-22T12:00:00Z', expires: null, jws: 'ab', entries: [{ type: 'Immunization', date: '2026-03-02', text: '207' }] }],
		import: { accepted: 1, queued: 0, skipped: 0 }
	};
	const cards = shcCards(flags);
	assert.equal(cards?.length, 1);
	assert.equal(cards?.[0].entries[0].text, '207');
	assert.equal(shcCards({ import: {} }), null);
	assert.equal(shcCards({ shc: 'not a list' }), null);
});

test('the receiver says who signed a card, never that the signer is trusted or verified', () => {
	const shc = (lang: string) => Object.entries(tables[lang]).filter(([k]) => k.startsWith('shc_'));
	for (const lang of ['en', 'es']) {
		assert.ok(shc(lang).length > 50, lang);
		for (const [k, v] of shc(lang)) {
			assert.doesNotMatch(v, /trust|verified|confianza|confiable|verificad/i, `${lang} ${k}: ${v}`);
		}
	}
	assert.equal(tables.en.shc_signed_by, 'Signed by');
	assert.equal(tables.es.shc_signed_by, 'Firmada por');
});

// SMART Health Link receive (slice 23)

test('every link refusal and every not-imported reason has an es and an en sentence', () => {
	for (const lang of ['en', 'es']) {
		for (const r of SHL_REASONS) assert.ok(tables[lang][linkRefusalKey(r, '1')], `${lang} ${r}`);
		for (const r of SHL_NOT_IMPORTED) assert.ok(tables[lang][notImportedKey(r)], `${lang} ${r}`);
		assert.ok(tables[lang][linkRefusalKey('passcode', null)], lang);
		assert.ok(tables[lang].shlr_r_other, lang);
	}
	assert.equal(linkRefusalKey('something_new', null), 'shlr_r_other');
	assert.equal(notImportedKey('something_new'), 'shlr_ni_unknown_type');
});

test('a wrong passcode says how many attempts are left, and a fetch detail never shows server text', () => {
	const en = words('en');
	assert.equal(
		fill(tables.en[linkRefusalKey('passcode', '3')], linkRefusalVars('passcode', '3', en)),
		'Not imported. The passcode is not right. The link allows 3 more attempts.'
	);
	assert.equal(fill(tables.es[linkRefusalKey('passcode', '3')], linkRefusalVars('passcode', '3', words('es'))), 'No importado. El código no es correcto. El enlace admite 3 intentos más.');
	assert.equal(linkRefusalKey('passcode', null), 'shlr_r_passcode_nocount');
	assert.match(linkRefusalVars('fetch', 'http_502', en).detail, /502/);
	assert.equal(linkRefusalVars('fetch', 'dns', en).detail, tables.en.shlr_d_dns);
	assert.equal(linkRefusalVars('fetch', 'shl.example does not resolve', en).detail, tables.en.shc_d_other);
});

test('a link is told from a card, alone or after a viewer URL', () => {
	assert.ok(isLink('shlink:/eyJ1cmwiOiJodHRwczovL3NobC5leGFtcGxlIn0'));
	assert.ok(isLink('https://viewer.example.org/#shlink:/eyJ1cmwiOiJ4In0'));
	assert.ok(!isLink('shc:/5676290952432060346029243740446031'));
});

test('the attempts left read right for one and for none, and none closes the question', () => {
	const sentence = (lang: string, left: string) => fill(tables[lang][linkRefusalKey('passcode', left)], linkRefusalVars('passcode', left, words(lang)));
	assert.equal(sentence('en', '1'), 'Not imported. The passcode is not right. The link allows 1 more attempt.');
	assert.equal(sentence('es', '1'), 'No importado. El código no es correcto. El enlace admite 1 intento más.');
	assert.equal(sentence('en', '0'), 'Not imported. The passcode is not right, and the link allows no more attempts, so it is closed now. Ask the sender for a new link.');
	assert.equal(sentence('es', '0'), 'No importado. El código no es correcto y el enlace ya no admite más intentos, así que ahora está cerrado. Pide un enlace nuevo a quien lo envió.');
	assert.equal(sentence('en', '2'), 'Not imported. The passcode is not right. The link allows 2 more attempts.');
	// the question and its passcode field stay for another try while the link is open
	assert.ok(passcodeRetry('passcode', '2'));
	assert.ok(passcodeRetry('passcode', null));
	assert.ok(!passcodeRetry('passcode', '0'));
	assert.ok(!passcodeRetry('inactive', null));
});

test('a link warns only about files it imported, and a file for someone else says so', () => {
	const fhir = [{ content_type: 'application/fhir+json' }];
	assert.equal(linkWarning('missing', [], [], 'Alex Rivera'), null, 'nothing was imported');
	assert.equal(linkWarning('matched', [], fhir, 'Alex Rivera'), null);
	const other = linkWarning('mismatch', [], fhir, 'Alex Rivera');
	assert.equal(other && fill(tables.en[other.key], other.vars), 'A file of this link names someone other than Alex Rivera. Its rows wait in review. If the link is for another person, open it from their profile.');
	assert.equal(other && fill(tables.es[other.key], other.vars), 'Un archivo de este enlace es de otra persona, no de Alex Rivera. Sus filas esperan en revisión. Si el enlace es para otra persona, ábrelo desde su perfil.');
	const unsure = linkWarning('hinted', [], fhir, 'Alex Rivera');
	assert.equal(unsure?.key, 'shlr_w_unsure');
	// a link of cards alone names the card, as a pasted card does
	const carmen = { name: 'Carmen Ortega', birth_date: '1955-01-30' };
	assert.equal(linkWarning('mismatch', [carmen], [{ content_type: 'application/smart-health-card' }], 'Alex Rivera')?.key, 'shc_w_other');
	// cards beside a FHIR file: one result for all the files, so no card is named
	assert.equal(linkWarning('mismatch', [carmen], [...fhir, { content_type: 'application/smart-health-card' }], 'Alex Rivera')?.key, 'shlr_w_other');
});

test('declining an issuer after the link was received says the link files stay stored', () => {
	assert.equal(declinedKey(0), 'shc_declined');
	const key = declinedKey(1);
	assert.equal(fill(tables.en[key], { host: 'issuer.labnorte.example' }), "Not imported. You declined issuer.labnorte.example, so nothing was fetched from it and nothing was imported. The link's files were already received: they stay stored with this profile, encrypted.");
	assert.equal(fill(tables.es[key], { host: 'issuer.labnorte.example' }), 'No se ha importado. Rechazaste issuer.labnorte.example, así que no se descargó nada de allí y no se importó nada. Los archivos del enlace ya se habían recibido: quedan guardados con este perfil, cifrados.');
});

test('a link whose files were all received before says nothing new was added', () => {
	assert.ok(receivedBefore([{ created: false }, { created: false }]));
	assert.ok(!receivedBefore([{ created: false }, { created: true }]));
	assert.ok(!receivedBefore([]));
	assert.ok(!receivedBefore(undefined));
	assert.equal(tables.en.shlr_again, 'These files were received before, so nothing new was added. The numbers are from the first import.');
	assert.equal(tables.es.shlr_again, 'Estos archivos ya se habían recibido, así que no se añadió nada nuevo. Las cifras son de la primera importación.');
});
