// A sync-gap alert (review kind source_gap): the server writes which connection stopped and what to do
// in es and en, and the review row shows that text in the reader's language.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { syncGapMessage } from '../src/lib/logic/inbox.ts';

const message = {
	en: 'Sync has stopped for Polar. The server operator can check Open Wearables and reconnect the affected accounts there.',
	es: 'Se ha interrumpido la sincronización de Polar. Quien administra el servidor puede revisar Open Wearables y volver a conectar allí las cuentas afectadas.'
};
const gap = (payload: unknown) => ({ kind: 'source_gap', payload });

test('the alert reads in the reader language, and in English for a language the server did not write', () => {
	assert.equal(syncGapMessage(gap({ message }), 'en'), message.en);
	assert.equal(syncGapMessage(gap({ message }), 'es'), message.es);
	assert.equal(syncGapMessage(gap({ message }), 'de'), message.en);
});

test('no text for another kind or a payload without a message', () => {
	assert.equal(syncGapMessage({ kind: 'job_failed', payload: { message } }, 'en'), '');
	for (const bad of [{}, null, { message: null }, { message: { en: 3 } }, { message: 'Polar' }]) {
		assert.equal(syncGapMessage(gap(bad), 'en'), '', JSON.stringify(bad));
	}
});
