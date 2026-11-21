// The Documents page groups each file under the year of the day it counts from, the same day the Done
// picker and the timeline show.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { documentYears } from '../src/lib/logic/documents.ts';

const doc = (id: string, collected_at: string | null, evidence_day: string | null) => ({ id, collected_at, evidence_day });

test('a report dated only by its report date sits under its year, and the undated files stay in one group', () => {
	// newest evidence first, as GET /profiles/{p}/documents sorts the list
	const rows = [
		doc('A_mamografia_solo_informe', null, '2026-11-08'),
		doc('C_mamografia', '2026-09-01T08:00:00Z', '2026-09-01'),
		doc('LabNorte_2025-07-15', '2025-07-15T08:00:00Z', '2025-07-15'),
		doc('D_sin_fecha', null, null),
		doc('scan', null, null)
	];
	assert.deepEqual(
		documentYears(rows, 'Undated').map(([y, l]) => [y, l.map((d) => d.id)]),
		[
			['2026', ['A_mamografia_solo_informe', 'C_mamografia']],
			['2025', ['LabNorte_2025-07-15']],
			['Undated', ['D_sin_fecha', 'scan']]
		]
	);
});
