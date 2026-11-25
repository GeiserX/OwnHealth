// Results shared by Garmin and Strava activity exports. GarminResult.svelte renders the counts
// and the files left out with their reasons.

export type ActivityExportSummary = {
	files_imported: number;
	gps_left_out: boolean;
	rows_without_file?: number;
	files_without_row?: number;
	not_imported: { member: string; reason: string }[];
};

/** The archive details of a job result or a document's flags. */
export function exportSummary(value: unknown): ActivityExportSummary | null {
	const details = value as { garmin?: unknown; strava?: unknown } | null | undefined;
	const summary = (details?.strava ?? details?.garmin) as Partial<ActivityExportSummary> | undefined;
	if (typeof summary?.files_imported !== 'number' || !Array.isArray(summary.not_imported)) return null;
	return {
		files_imported: summary.files_imported,
		gps_left_out: summary.gps_left_out === true,
		not_imported: summary.not_imported,
		...(typeof summary.rows_without_file === 'number' && { rows_without_file: summary.rows_without_file }),
		...(typeof summary.files_without_row === 'number' && { files_without_row: summary.files_without_row })
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

export function isActivityExportJob(kind: string | undefined): boolean {
	return kind === 'import.garmin_zip' || kind === 'import.strava_zip';
}

/** A finished import that read no file of readings: its pill says so instead of "Imported". */
export function exportNothing(job: { kind?: string; state?: string; result?: unknown } | undefined): boolean {
	if (!job || !isActivityExportJob(job.kind) || job.state !== 'done') return false;
	const summary = exportSummary(job.result);
	return summary !== null && summary.files_imported === 0;
}
