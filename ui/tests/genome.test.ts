// The Genome screen's PGx gene grid and its chip note, in both languages.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dict } from '../src/lib/i18n/dict.ts';
import { COVERAGE_REASONS, genomeEmpty, gridCall, limitLines, resultCell, svNote, type GridRow } from '../src/lib/logic/genome.ts';

const tr = (lang: 'en' | 'es') => (k: string) => (dict[lang] as Record<string, string>)[k] ?? k;
const en = tr('en');
const es = tr('es');

const row = (extra: Partial<GridRow>): GridRow => ({
	status: 'found',
	diplotype: null,
	phenotype: null,
	markers: [],
	coverage: { state: 'assessed', reason: null },
	sv_coverage: null,
	...extra
});

test('an absence on partial coverage reads as partial with its reason, never as an assessed negative', () => {
	const r = row({ status: 'partial', coverage: { state: 'partial', reason: 'paralog' } });
	assert.equal(gridCall(r, en), 'partial coverage · a similar gene nearby (paralog)');
	assert.equal(gridCall(r, es), 'cobertura parcial · un gen parecido cerca (parálogo)');
	assert.notEqual(gridCall(r, en), en('gen_grid_no_finding'));
});

test('a coverage reason is words in the reader language, never the code', () => {
	const r = row({ status: 'not_assessed', coverage: { state: 'not_assessed', reason: 'no_bam' } });
	assert.equal(gridCall(r, es), 'no evaluado · sin archivo de alineamiento (BAM)');
	assert.equal(gridCall(r, en), 'not assessed · no alignment file (BAM)');
	// a code we have no words for shows nothing of it
	const odd = row({ status: 'not_assessed', coverage: { state: 'not_assessed', reason: 'brand_new_code' } });
	assert.equal(gridCall(odd, en), 'not assessed');
	for (const code of COVERAGE_REASONS) {
		for (const lang of ['en', 'es'] as const) {
			const s = (dict[lang] as Record<string, string>)['gen_grid_reason_' + code];
			assert.ok(s && !s.includes('_'), `${lang} gen_grid_reason_${code}`);
		}
	}
});

test('structural variants not assessed show under their gene, assessed ones say nothing', () => {
	const r = row({ diplotype: '*1/*41', sv_coverage: { state: 'not_assessed', reason: 'no_bam' } });
	assert.equal(svNote(r, en), 'structural variants: not assessed · no alignment file (BAM)');
	assert.equal(svNote(r, es), 'variantes estructurales: no evaluado · sin archivo de alineamiento (BAM)');
	assert.equal(svNote(row({ sv_coverage: { state: 'assessed', reason: null } }), en), null);
	assert.equal(svNote(row({}), en), null);
});

test('the chip note is about very rare variants, so it never reads as the chance of a common call', () => {
	for (const lang of ['en', 'es'] as const) {
		const s = (dict[lang] as Record<string, string>).gen_chip_note;
		const common = lang === 'en' ? 'common variants' : 'variantes comunes';
		const rare = lang === 'en' ? 'very rare' : 'muy raras';
		assert.ok(s.includes(common) && s.includes(rare) && s.includes('{pct}'), `${lang}: ${s}`);
		// the cell already says a clinical test confirms it (i2_confirm_clin): the note does not repeat it
		assert.ok(!/confirm(s|a) or rules|lo confirma o lo descarta/.test(s), `${lang}: ${s}`);
	}
});

test('a refused PRS reads not interpretable even when its fact carries a phenotype', () => {
	const tf = (k: string, vars: Record<string, string>) => Object.entries(vars).reduce((s, [a, b]) => s.replace(`{${a}}`, b), en(k));
	const prs = { category: 'prs', diplotype: null, phenotype: 'High', value: { percentile: 91 }, prs: { status: 'not_interpretable', reason_code: 'low_variant_match' } };
	assert.equal(resultCell(prs, en, tf), en('gen_prs_ni'));
	const ok = { ...prs, phenotype: null, prs: { status: 'interpretable', reason_code: null }, value: { percentile: 91, most_similar_pop: 'EUR' } };
	assert.ok(resultCell(ok, en, tf).includes('91'));
	const pgx = { category: 'pgx', diplotype: '*1/*2', phenotype: 'Intermediate Metabolizer', value: {} };
	assert.equal(resultCell(pgx, en, tf), '*1/*2 · Intermediate Metabolizer');
});

test('a dataset whose only call sits below the table level still shows the grid, not the import card', () => {
	const base = { findings: [], locked: [], withheld: { minor_rule: 0 }, pgx_grid: [] as { status: string }[] };
	assert.equal(genomeEmpty({ ...base, pgx_grid: [{ status: 'found' }, { status: 'not_assessed' }] }, true), false);
	assert.equal(genomeEmpty({ ...base, pgx_grid: [{ status: 'not_assessed' }, { status: 'no_finding' }] }, true), true);
	assert.equal(genomeEmpty({ ...base, pgx_grid: [{ status: 'found' }] }, false), true);
});

test('an HLA marker whose presence the file does not state reads unknown, never absent', () => {
	const r = row({ markers: [{ marker: 'HLA-B*58:01', present: null }, { marker: 'HLA-B*15:02', present: false }] });
	assert.equal(gridCall(r, en), 'B*58:01 not determined · B*15:02 absent');
	assert.equal(gridCall(r, es), 'B*58:01 no determinado · B*15:02 ausente');
});

test('the finding table reads an unstated HLA marker as not determined too', () => {
	const tf = (k: string) => en(k);
	const hla = (present: unknown) => ({ category: 'hla', value: { alleles: ['HLA-B*07:02'], marker: 'HLA-B*58:01', present } });
	assert.equal(resultCell(hla(null), en, tf), 'HLA-B*07:02 · HLA-B*58:01 not determined');
	assert.equal(resultCell(hla(false), es, tf), 'HLA-B*07:02 · HLA-B*58:01 ausente');
	assert.equal(resultCell(hla(true), en, tf), 'HLA-B*07:02 · HLA-B*58:01 present');
});

test('the stale texts name the OwnGenome command that re-runs the out-of-date steps', () => {
	for (const lang of ['en', 'es'] as const) {
		const d = dict[lang] as Record<string, string>;
		for (const k of ['gen_stale_title', 'gen_stale_steps']) {
			assert.ok(d[k].includes('bin/og recheck --sample <name> --rerun'), `${lang} ${k}: ${d[k]}`);
		}
		assert.ok(d.gen_stale_title.includes('{by}') && d.gen_stale_title.includes('{steps}'), `${lang}: ${d.gen_stale_title}`);
	}
});

test('two datasets with the same limitation show it once, so the keyed list never repeats a key', () => {
	const tf = (k: string, v: Record<string, string>) => `${en(k)}|${v.kind}`;
	const lines = limitLines(
		['No VCF uploaded', 'No VCF uploaded', 'Free text'],
		[{ code: 'no_vcf', assay: 'wgs' }, { code: 'no_vcf', assay: 'wgs' }, null],
		tf
	);
	assert.equal(lines.length, 2);
	assert.equal(new Set(lines).size, lines.length);
	assert.equal(lines[1], 'Free text');
});
