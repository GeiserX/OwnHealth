// Calculators on the biomarker page: which results a page lists, and the newest one of each.
import type { CalcItem } from '../api/types';
import { unitLabel } from './format.ts';

export interface CalcSummary {
	id: string;
	/** the newest result: the latest draw, or the one item that says why there is none */
	latest: CalcItem;
	/** how many draws gave a value */
	computed: number;
}

/** One summary per calculator whose item matches, in the server's order. Items of one calculator come oldest first. */
export function summarize(items: CalcItem[], match: (i: CalcItem) => boolean): CalcSummary[] {
	const out: CalcSummary[] = [];
	for (const it of items) {
		if (!match(it)) continue;
		const s = out.find((x) => x.id === it.id);
		if (!s) out.push({ id: it.id, latest: it, computed: it.status === 'ok' ? 1 : 0 });
		else {
			s.latest = it;
			if (it.status === 'ok') s.computed++;
		}
	}
	return out;
}

/** The calculators listed on one marker's page. */
export const forGroup = (items: CalcItem[], group: string) => summarize(items, (i) => i.shows_on.includes(group));

/** The computed rows a dashboard card shows beside its markers (display only, no status word). */
export const rowsForCard = (items: CalcItem[], card: string) =>
	summarize(items, (i) => i.system === card).filter((s) => s.latest.status === 'ok');

/** A unit for people: 'mL/min/{1.73_m2}' reads 'mL/min/1.73 m²', 'u[IU]/mL' reads 'µU/mL' (Matthews' unit). */
export const calcUnit = (u: string | null | undefined) =>
	unitLabel(u).replace('{1.73_m²}', '1.73 m²').replace('{1.73_m2}', '1.73 m²').replace('u[IU]/mL', 'µU/mL').replace('m[IU]/L', 'mU/L');
