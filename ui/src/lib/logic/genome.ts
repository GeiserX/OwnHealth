// The Genome screen's PGx gene grid: the words of one gene box, in the reader's language.

/** The parts of a pgx_grid row these helpers read. */
export interface GridRow {
	status: 'found' | 'no_finding' | 'partial' | 'not_assessed' | 'withheld';
	diplotype: string | null;
	phenotype: string | null;
	markers: { marker: string; present: boolean }[];
	coverage: { state: string; reason: string | null } | null;
	sv_coverage?: { state: string; reason: string | null } | null;
}

type T = (key: string) => string;

/** The coverage reasons OwnGenome writes (genomics.md, genome-facts coverage[].reason). */
export const COVERAGE_REASONS = [
	'not_on_array',
	'low_depth',
	'paralog',
	'sv_not_callable',
	'no_bam',
	'required_positions_missing',
	'not_run',
	'assay_cannot_detect',
	'other'
] as const;

/** A coverage reason in the reader's words; null for a code we have no words for (never the raw code). */
export function reasonText(code: string | null | undefined, t: T): string | null {
	if (!code) return null;
	const k = 'gen_grid_reason_' + code;
	const s = t(k);
	return s === k ? null : s;
}

/** A coverage state and its reason: "partial coverage · a similar gene nearby (paralog)". */
function coverageText(state: string, reason: string | null | undefined, t: T): string {
	const word = state === 'partial' ? t('gen_grid_partial') : t('gen_grid_not_assessed');
	return [word, reasonText(reason, t)].filter(Boolean).join(' · ');
}

/** The call of one gene box, or why there is none. */
export function gridCall(r: GridRow, t: T): string {
	if (r.status === 'withheld') return t('gen_grid_withheld');
	if (r.status === 'no_finding') return t('gen_grid_no_finding');
	if (r.status === 'partial') return coverageText('partial', r.coverage?.reason, t);
	if (r.status === 'not_assessed') {
		const why = r.coverage?.reason;
		return why === 'not_in_coverage_map' ? t('gen_grid_not_in_map') : coverageText('not_assessed', why, t);
	}
	if (r.markers.length && !r.phenotype) {
		return r.markers.map((m) => `${m.marker.replace(/^HLA-/, '')} ${m.present ? t('i2_present') : t('i2_absent')}`).join(' · ');
	}
	return [r.diplotype, r.phenotype].filter(Boolean).join(' · ') || '—';
}

/** The gene's structural variants when they were not fully assessed (CYP2D6 from a chip or without a BAM). */
export function svNote(r: GridRow, t: T): string | null {
	const sv = r.sv_coverage;
	if (!sv || sv.state === 'assessed') return null;
	return `${t('gen_grid_sv')}: ${coverageText(sv.state, sv.reason, t)}`;
}

/** The parts of a finding the result cell reads. */
export interface ResultRow {
	category: string;
	diplotype?: string | null;
	phenotype?: string | null;
	zygosity?: string | null;
	condition?: { name?: string | null } | null;
	value?: Record<string, unknown> | null;
	prs?: { status: string; reason_code: string | null } | null;
}

type TF = (key: string, vars: Record<string, string>) => string;

/** The result cell of one finding. A refused PRS says so before anything its fact carries. */
export function resultCell(f: ResultRow, t: T, tf: TF): string {
	const v = (k: string) => f.value?.[k] as string | number | boolean | string[] | undefined;
	if (f.category === 'prs' && f.prs?.status === 'not_interpretable') return t('gen_prs_ni');
	if (f.diplotype || f.phenotype) return [f.diplotype, f.phenotype].filter(Boolean).join(' · ');
	if (f.category === 'prs') return tf('i2_prs_pct', { p: String(v('percentile') ?? '—'), pop: String(v('most_similar_pop') ?? '') });
	if (f.category === 'hla') return `${(v('alleles') as string[] | undefined)?.join(' · ') ?? ''} · ${v('marker')} ${v('present') ? t('i2_present') : t('i2_absent')}`;
	if (f.category === 'trait') return `${v('trait') ?? ''} ${v('call') ?? ''}`.trim();
	return [f.zygosity ? t('i2_zyg_' + f.zygosity) : null, f.condition?.name].filter(Boolean).join(' · ');
}
