// Calculators on the biomarker page: the pure helpers that pick what a page lists.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { calcUnit, forGroup, rowsForCard } from '../src/lib/logic/calc.ts';
import type { CalcItem } from '../src/lib/api/types.ts';

const item = (id: string, status: CalcItem['status'], date: string | undefined, value?: string): CalcItem => ({
	id,
	version: id,
	name: id,
	short: id,
	status,
	value,
	date,
	inputs: [],
	cite: 'https://pubmed.ncbi.nlm.nih.gov/34554658/',
	src: [],
	notes: [],
	system: id.startsWith('ckd') ? 'kidney' : 'liver',
	shows_on: id.startsWith('ckd') ? ['creatinine', 'egfr'] : ['alt', 'ast']
});

const items = [
	item('ckd_epi_2021_cr', 'ok', '2025-01-20', '97'),
	item('ckd_epi_2021_cr', 'ok', '2026-09-14', '96'),
	item('ckd_epi_2021_cr_cys', 'not_assessed', undefined),
	item('fib4', 'refused', '2026-09-14')
];

test('a marker page lists the calculators that read it, each with its newest result', () => {
	const s = forGroup(items, 'creatinine');
	assert.deepEqual(
		s.map((x) => [x.id, x.latest.value ?? x.latest.status, x.computed]),
		[
			['ckd_epi_2021_cr', '96', 2],
			['ckd_epi_2021_cr_cys', 'not_assessed', 0]
		]
	);
	assert.deepEqual(forGroup(items, 'ldl_c'), []);
});

test('a dashboard card shows only computed results, never a refusal as a value', () => {
	assert.deepEqual(rowsForCard(items, 'kidney').map((x) => x.id), ['ckd_epi_2021_cr']);
	assert.deepEqual(rowsForCard(items, 'liver'), []);
});

test('the eGFR unit reads as people write it', () => {
	assert.equal(calcUnit('mL/min/{1.73_m2}'), 'mL/min/1.73 m²');
	assert.equal(calcUnit('u[IU]/mL'), 'µU/mL');
	assert.equal(calcUnit('10*9/L'), '×10⁹/L');
	assert.equal(calcUnit(null), '');
});
