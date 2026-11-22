// The Activity log shows an imaging.open row in words, in both languages, and never the study UID or
// the hashed session marker the row carries (imaging_proxy.rs writes them for the one-row-per-session rule).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dict } from '../src/lib/i18n/dict.ts';
import { auditActionKey, auditDetail } from '../src/lib/logic/people.ts';

const row = {
	via: 'ui',
	tool: null,
	action: 'imaging.open',
	args_summary: 'h:0123456789ab',
	resource_type: 'imaging_study',
	resource_ids: ['1.2.826.0.1.3680043.10.999.1']
};

for (const [lang, opened, study] of [
	['en', 'Imaging study opened', 'Imaging study'],
	['es', 'Estudio de imagen abierto', 'Estudio de imagen']
] as const) {
	test(`activity: an opened imaging study reads as words in ${lang}`, () => {
		const table = dict[lang] as Record<string, string>;
		const t = (k: string) => table[k] ?? k;
		const key = auditActionKey(row);
		assert.equal(key, 'i2_act_imaging_open');
		assert.equal(t(key ?? ''), opened);
		const detail = auditDetail(row, { t, scope: (s) => s, n: (k, n) => `${n} ${k}` });
		assert.equal(detail, study);
		assert.ok(!detail.includes('1.2.826') && !detail.includes('h:'), detail);
	});
}
