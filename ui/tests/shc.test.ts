// SMART Health Card receive (slice 22): every refusal reason and detail code has a sentence in both
// languages, a card for someone else says whose it is, and the receiver never calls a signer trusted
// or verified.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { SHC_DETAILS, SHC_REASONS, detailKey, identityWarning, refusalKey, refusalVars, shcCards } from '../src/lib/logic/shc.ts';
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
