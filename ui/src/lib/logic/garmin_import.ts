// The result of a Garmin account export import (job import.garmin_zip, slice 30): the files it read, the
// readings they added, and the files left out with their reasons. Pure: tests/garmin_import.test.ts runs
// it under node; GarminResult.svelte turns it into text with t/tp.

export type GarminSummary = {
	files_imported: number;
	gps_left_out: boolean;
	not_imported: { member: string; reason: string }[];
};

/** The `garmin` details of a job result or of a document's flags; null for anything else. */
export function garminSummary(value: unknown): GarminSummary | null {
	const summary = value as Partial<GarminSummary> | null;
	if (typeof summary?.files_imported !== 'number' || !Array.isArray(summary.not_imported)) return null;
	return {
		files_imported: summary.files_imported,
		gps_left_out: summary.gps_left_out === true,
		not_imported: summary.not_imported
	};
}

const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);

/** What an import added: the new readings, or that every reading was already there (the same export
 * sent again). Null when the result counts no reading (a document's flags, or nothing usable). */
export function garminReadings(result: unknown): { key: string; n: number } | null {
	if (!result || typeof result !== 'object') return null;
	const r = result as Record<string, unknown>;
	const accepted = num(r.accepted);
	if (accepted > 0) return { key: 'i2_garmin_readings', n: accepted };
	const duplicate = num((r.reasons as Record<string, unknown> | undefined)?.duplicate);
	return duplicate > 0 ? { key: 'i2_garmin_nothing_new', n: duplicate } : null;
}

/** A finished import that read no file of readings: its pill says so instead of "Imported". */
export function garminNothing(job: { kind?: string; state?: string; result?: unknown } | undefined): boolean {
	if (!job || job.kind !== 'import.garmin_zip' || job.state !== 'done') return false;
	const summary = garminSummary((job.result as Record<string, unknown> | null | undefined)?.garmin);
	return summary !== null && summary.files_imported === 0;
}
