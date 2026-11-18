// Pure chart helpers (no Svelte, no imports), so node --test can run them: ui/tests/*.test.ts.

/** 1, 2, 2.5, 5 or 10 times a power of ten, the smallest not below `raw`. */
export function niceStep(raw: number): number {
	const p = 10 ** Math.floor(Math.log10(raw));
	const m = raw / p;
	return (m <= 1 ? 1 : m <= 2 ? 2 : m <= 2.5 ? 2.5 : m <= 5 ? 5 : 10) * p;
}

/**
 * Y axis for a wearables chart. `floor` fixes the bottom first (0 for bars, a whole number for sleep),
 * then the step comes from the range the axis really spans, so it keeps about 4-5 ticks.
 */
export function axisTicks(
	nums: number[],
	floor?: number,
	limits: { max?: number; minSpan?: number } = {}
): { lo: number; hi: number; step: number; ticks: number[] } {
	// `minSpan` widens a flat series around its middle, so a 1-unit change is not drawn as a cliff;
	// `max` is a ceiling the measure cannot pass (SpO2 100 %): the widening goes down instead.
	let min = Math.min(...nums);
	let max = Math.max(...nums);
	if (limits.minSpan && max - min < limits.minSpan) {
		const mid = (min + max) / 2;
		min = mid - limits.minSpan / 2;
		max = mid + limits.minSpan / 2;
		if (limits.max != null && max > limits.max) {
			min -= max - limits.max;
			max = limits.max;
		}
	}
	let lo: number;
	let hi: number;
	let step: number;
	if (floor !== undefined) {
		lo = Math.min(floor, min);
		step = niceStep((max - lo || Math.abs(max) || 1) / 4);
		hi = lo + Math.max(1, Math.ceil((max - lo) / step - 1e-9)) * step;
	} else {
		step = niceStep((max - min || Math.abs(max) || 1) / 4);
		lo = Math.floor((min - step * 0.4) / step) * step;
		hi = Math.ceil((max + step * 0.2) / step) * step;
	}
	if (limits.max != null && hi > limits.max) hi = Math.max(limits.max, lo + step);
	const ticks: number[] = [];
	for (let v = lo; v <= hi + 1e-9; v += step) ticks.push(+v.toFixed(4));
	return { lo, hi, step, ticks };
}

export type Annotation = { date: string; end: string | null; label: string };
export type Bar = { x: number; x2: number; span: boolean; ongoing: boolean; label: string; row: number };

/**
 * Medication courses for the biomarker chart. A course with no end is still running: it draws as a
 * bar to `asOf` on the Meds row. Only a one-day annotation (end equal to its date) is an event dot.
 * `start` is the first date of the axis.
 */
export function buildBars(annotations: Annotation[], clampX: (d: string) => number, asOf: string, start?: string): Bar[] {
	// Anything that ended before the axis starts is outside the chart: drop it, never a sliver at the edge.
	const shown = start ? annotations.filter((a) => (a.end ?? asOf) >= start) : annotations;
	const bars = shown.map((a) => {
		const ongoing = a.end == null;
		return { x: clampX(a.date), x2: clampX(a.end ?? asOf), span: ongoing || a.end !== a.date, ongoing, label: a.label, row: 0 };
	});
	// Courses that overlap in time go on separate rows, so two current medications never print over each other.
	const rowEnd: number[] = [];
	for (const b of bars.filter((b) => b.span).sort((a, b) => a.x - b.x)) {
		let r = rowEnd.findIndex((end) => b.x > end + 2);
		if (r < 0) r = rowEnd.length;
		rowEnd[r] = Math.max(b.x2, b.x + 4);
		b.row = r;
	}
	return bars;
}

export type Mark = { x: number; label: string };
export type PlacedMark = Mark & { row: number; anchor: 'start' | 'end'; tx: number };

/**
 * The dashed lab and method change markers of the biomarker chart. A label starts 4 units right of its
 * line, unless it would pass `right` (the plot's right edge): then it ends 4 units left of the line, so
 * a change at the latest point stays readable. Each label takes the first row where it does not print
 * over an earlier one (about 5.6 units per character at font size 10).
 */
export function layoutMarks(marks: Mark[], right: number): PlacedMark[] {
	const rowEnd: number[] = [];
	return marks.map((m) => {
		const w = m.label.length * 5.6;
		const end = m.x + 4 + w > right;
		const tx = end ? m.x - 4 : m.x + 4;
		const [a, b] = end ? [tx - w, tx] : [tx, tx + w];
		let row = rowEnd.findIndex((e) => a > e + 6);
		if (row < 0) row = rowEnd.length;
		rowEnd[row] = b;
		return { ...m, row, anchor: end ? 'end' : 'start', tx };
	});
}

/** The labs of a series in the order first seen (null: entered by hand), and how many are real labs. */
export function labsOf(points: { lab?: string | null }[]): { keys: (string | null)[]; count: number } {
	const keys = [...new Set(points.map((p) => p.lab ?? null))];
	return { keys, count: keys.filter((k) => k != null).length };
}

/** The detail page subtitle: a series with no lab (every value entered by hand) leaves the lab count out. */
export function measKey(labCount: number): 'h1_meas' | 'h1_meas_nolab' {
	return labCount > 0 ? 'h1_meas' : 'h1_meas_nolab';
}
