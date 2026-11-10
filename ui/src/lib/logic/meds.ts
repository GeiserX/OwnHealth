// The Medications screen: which medicine rows are current, the history of each medicine, and its code chips.

/** The parts of a medication RecordOut these helpers read. */
export interface MedRecord {
	id: string;
	title: string;
	detail: Record<string, unknown>;
	status: string;
	status_reason: string | null;
	supersedes: string | null;
	recorded_at: string;
}

const str = (v: unknown): string | null => (typeof v === 'string' && v.trim() ? v : null);

/**
 * Taken on `today` (YYYY-MM-DD), as the server's active_medications counts it: med_status active within
 * its dates, or a stopped course whose dates hold today. Only the record's current version counts.
 */
export function isCurrent(r: MedRecord, today: string): boolean {
	if (r.status !== 'current' && r.status !== 'provisional') return false;
	const started = str(r.detail.started);
	const stopped = str(r.detail.stopped);
	const status = str(r.detail.med_status) ?? 'active';
	const startedOk = !started || started <= today;
	const stoppedOk = !stopped || stopped > today;
	return (status === 'active' && startedOk && stoppedOk) || (status !== 'on_hold' && !!started && !!stopped && startedOk && stoppedOk);
}

export interface MedGroup<T extends MedRecord = MedRecord> {
	key: string;
	title: string;
	current: boolean;
	/** Newest course first; corrected and retracted versions sit with their medicine. */
	entries: T[];
}

/** Every version of every medicine, one group per ingredient (else name): current medicines first. */
export function historyGroups<T extends MedRecord>(records: T[], today: string): MedGroup<T>[] {
	const byKey = new Map<string, T[]>();
	for (const r of records) {
		const key = (str(r.detail.ingredient) ?? str(r.detail.name) ?? r.title).trim().toLowerCase();
		byKey.set(key, [...(byKey.get(key) ?? []), r]);
	}
	const since = (r: MedRecord) => str(r.detail.started) ?? '';
	const groups: MedGroup<T>[] = [...byKey].map(([key, rows]) => {
		const entries = [...rows].sort((a, b) => since(b).localeCompare(since(a)) || b.recorded_at.localeCompare(a.recorded_at));
		return { key, title: str(entries[0].detail.name) ?? entries[0].title, current: entries.some((e) => isCurrent(e, today)), entries };
	});
	return groups.sort((a, b) => Number(b.current) - Number(a.current) || since(b.entries[0]).localeCompare(since(a.entries[0])) || a.title.localeCompare(b.title));
}

const SYSTEMS: Record<string, string> = {
	rxnorm: 'RxNorm',
	'http://www.nlm.nih.gov/research/umls/rxnorm': 'RxNorm',
	atc: 'ATC',
	'http://www.whocc.no/atc': 'ATC',
	snomed: 'SNOMED',
	'http://snomed.info/sct': 'SNOMED'
};
const FULL: Record<string, string> = { rxnorm: 'http://www.nlm.nih.gov/research/umls/rxnorm', atc: 'http://www.whocc.no/atc', snomed: 'http://snomed.info/sct' };

/** A short name for a code system: ours, else the host of its URL, else the system as sent. */
function systemLabel(system: string | null): string {
	if (!system) return '';
	const known = SYSTEMS[system.toLowerCase()];
	if (known) return known;
	try {
		return new URL(system).hostname.replace(/^www\./, '');
	} catch {
		return system;
	}
}

export interface CodeChip {
	label: string;
	code: string;
	display: string | null;
	system: string | null;
}

/** The primary code first, then every other code the source sent, each once. */
export function codeChips(detail: Record<string, unknown>): CodeChip[] {
	const out: CodeChip[] = [];
	const seen = new Set<string>();
	const add = (system: string | null, code: string | null, display: string | null) => {
		if (!code) return;
		const full = system ? (FULL[system.toLowerCase()] ?? system) : '';
		const k = `${full}|${code}`;
		if (seen.has(k)) return;
		seen.add(k);
		out.push({ label: systemLabel(system), code, display, system });
	};
	add(str(detail.code_system), str(detail.code), null);
	if (Array.isArray(detail.codes)) {
		for (const c of detail.codes as Record<string, unknown>[]) add(str(c?.system), str(c?.code), str(c?.display));
	}
	return out;
}
