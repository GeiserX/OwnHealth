// REST API types. This file mirrors CONTRACTS.md "REST API": change both together.
// Ids are UUID strings, dates ISO 8601 strings, decimals arrive as JSON numbers or strings.

export type Uuid = string;
export type IsoDate = string; // 2026-11-07
export type IsoDateTime = string; // 2026-11-07T07:12:00Z
export type Role = 'owner' | 'guardian' | 'caregiver' | 'viewer';
export type Status = 'ok' | 'watch' | 'out' | 'stale' | 'nodata';
export type Lifecycle = 'provisional' | 'current' | 'superseded' | 'retracted';
export type Scope =
	| 'read:summary'
	| 'read:labs'
	| 'read:vitals'
	| 'read:wearables'
	| 'read:records'
	| 'read:genome'
	| 'read:genome_raw'
	| 'read:sensitive'
	| 'read:documents_text'
	| 'write:propose';

/** Free text that came from a document, a person or an agent. Render as text, never as HTML. */
export interface UntrustedText {
	trust: 'untrusted_text';
	origin: string;
	text: string;
}

export interface ApiErrorBody {
	detail: { code: string; message: string; allowed?: string[] } | string;
}

// ---------------------------------------------------------------- foundation ----

export interface ProfileSummary {
	id: Uuid;
	slug: string;
	display_name: string;
	initials: string;
	role: Role;
	relationship: string | null;
	age: number | null;
	sex_at_birth: 'female' | 'male' | 'unknown' | null;
	country: string | null;
	is_minor: boolean;
	is_self: boolean;
	unit_pref: 'si' | 'conventional' | 'as_reported';
	genome_sensitive_ui: boolean;
	scopes: Scope[];
	flags?: number; // added by lane E when the status snapshot exists
}

export interface Me {
	user: { id: Uuid; display_name: string; locale: 'es' | 'en' };
	household: { id: Uuid; name: string };
	auth: { kind: 'session' | 'token' | 'dev' };
	profiles: ProfileSummary[];
	review_open: number;
	demo: boolean;
}

export interface SourceDetail {
	src_id: string;
	kind: string;
	label: string | null;
	status: Lifecycle | 'gone';
	facts: { table: string; id: Uuid; status: Lifecycle; status_reason: string | null }[];
	document: {
		id: Uuid;
		profile_id: Uuid | null;
		doc_type: string | null;
		mime: string;
		pages: number | null;
		provider_name: string | null;
		report_status: string | null;
		collected_at: IsoDateTime | null;
		reported_at: IsoDateTime | null;
		sha256: string;
		status: string;
	} | null;
	location: { page: number; bbox: [number, number, number, number] | null; units: 'pt' } | null;
	import_ref: string | null;
	created_at: IsoDateTime;
	text: UntrustedText | null;
}

export interface Check {
	name: string;
	ok: boolean;
	reason: string | null;
	detail: string | null;
}

export interface ReviewItem {
	id: Uuid;
	kind:
		| 'candidate_row'
		| 'agent_proposal'
		| 'fhir_unmapped'
		| 'document_identity'
		| 'amended_report'
		| 'rederive_diff'
		| 'genome_reinterpretation'
		| 'job_failed'
		| 'restore_check_failed';
	profile_id: Uuid | null;
	status: 'open' | 'accepted' | 'rejected' | 'dismissed';
	reasons: string[];
	document_id: Uuid | null;
	candidate_row_id: Uuid | null;
	observation_id: Uuid | null;
	/** candidate_row: {observation, src, checks: Check[], cells, page_no, bbox, row_conf, why_not, allowance} */
	payload: Record<string, unknown>;
	row_conf: number | null;
	proposed_by_token: Uuid | null;
	context_src_ids: string[] | null;
	decided_by: Uuid | null;
	decided_at: IsoDateTime | null;
	decision_reason: string | null;
	result_ids: Uuid[] | null;
	created_at: IsoDateTime;
}

export interface ImportItem {
	filename: string;
	kind: 'pdf' | 'image' | 'apple_health_zip' | 'fhir_json' | 'fhir_ndjson' | 'genome_facts' | 'vcf' | 'tbi';
	sha256: string;
	document_id?: Uuid;
	duplicate?: boolean;
	job_id?: number | null;
	attached_to?: string;
}

export interface JobInfo {
	id: number;
	kind: string;
	state: 'queued' | 'running' | 'done' | 'failed' | 'dead';
	attempts: number;
	max_attempts: number;
	last_error: string | null;
	result: Record<string, unknown> | null;
	created_at: IsoDateTime;
	finished_at: IsoDateTime | null;
	profile_id: Uuid | null;
	filename: string | null;
	document_id: Uuid | null;
}

// ------------------------------------------------- lane E: status, dashboard ----

export interface MarkerRow {
	alias_group: string;
	name: string; // localized display name
	value: string; // as printed, or canonical in the chosen units
	unit: string;
	status: Status;
	stale: boolean;
	date: IsoDate;
	src: string; // src id
	src_label: string; // 'LabNorte · 2026-09-14 · p2'
	spark: number[];
	band: [number, number] | null;
}

export interface SystemCard {
	key: string; // cardiometabolic | liver | kidney | thyroid | blood | inflammation | vitamins | sleep | fitness
	name: string;
	status: Status;
	markers: MarkerRow[];
	newest: IsoDate | null;
	rule: string; // 'cardiometabolic.yaml'
}

export interface MedChip {
	name: string;
	strength: string | null;
	since: IsoDate | null;
	src: string;
	pgx: { gene: string; label: string; phenotype: string; level: string | null; text: string } | null;
}

export interface Dashboard {
	profile: string;
	as_of: IsoDate;
	subtitle: string;
	systems: SystemCard[];
	medications: MedChip[];
	changes: { name: string; from: string; to: string; unit: string; direction: 'up' | 'down'; status: Status }[];
	changes_vs: IsoDate | null;
	not_measured_this_time: string[];
	due: { title: string; why: string; status: Status; kind: 'lab' | 'vaccine' | 'screening'; phase: string | null }[];
	genome: { summary: string; chips: string[]; hidden: number; source: string | null } | null;
	review_open: number;
	state_hash: string;
	rules_version: string;
	generated_at: IsoDateTime;
}

// ------------------------------------------------ lane A: observations, trend ----

export interface ObservationOut {
	id: Uuid;
	src: string;
	src_label: string | null;
	name: string;
	printed: { name: string; value: string; unit: string | null; range: string | null; flag: string | null };
	value: number | null;
	unit: string | null;
	loinc: string | null;
	alias_group: string;
	interpretation: 'L' | 'N' | 'H' | 'LL' | 'HH' | 'A' | null;
	method: string | null;
	date: IsoDate;
	lab: string | null;
	accepted_by: 'auto' | 'person' | 'import';
	status: Lifecycle;
}

export interface TrendPoint {
	date: IsoDate;
	value: number;
	unit: string;
	printed: string; // raw printed string, shown on hover
	range: string | null;
	ref_low: number | null;
	ref_high: number | null;
	flag: string | null;
	method: string | null;
	loinc: string | null;
	lab: string | null;
	src: string;
	src_label: string | null;
}

export interface Trend {
	name: string;
	alias_group: string;
	codes: string[];
	unit: string;
	unit_options: string[];
	points: TrendPoint[];
	annotations: { date: IsoDate; end: IsoDate | null; label: string; src: string }[];
	stats: { n: number; last_vs_previous_pct: number | null; rcv_pct: number | null; exceeds_rcv: boolean | null };
	limitations: string[];
}

// ------------------------------------------------------- lane D: genome ----

export type CallState = 'ref' | 'alt' | 'no_call' | 'not_on_array' | 'not_covered';

export interface GenomeFinding {
	id: string; // f_ + 16 hex
	category: string;
	gene: string | null;
	title: string;
	diplotype: string | null;
	phenotype: string | null;
	rsid: string | null;
	zygosity: string | null;
	evidence: 'E1' | 'E2' | 'E3' | 'E4';
	confidence: 'high' | 'medium' | 'low' | 'unconfirmed';
	call_state: CallState | null;
	verification: 'provisional' | 'confirmed' | 'refuted';
	summary: UntrustedText | null;
	drugs: string[];
	src: string;
}

export interface GenomeOverview {
	dataset: {
		assay: string;
		build: string;
		source_tool: string;
		imported_at: IsoDateTime;
		db_versions: Record<string, string>;
	} | null;
	tiles: { key: string; value: string; detail: string }[];
	findings: GenomeFinding[];
	coverage: { target: string; panel: string | null; state: 'assessed' | 'partial' | 'not_assessed'; reason: string | null }[];
	withheld: { sensitive: number; minor_rule: number };
	limitations: string[];
}

// --------------------------------------------------- lane E: drug check ----

export interface DrugCheck {
	drug: { name: string; cpic_drug_id: string | null; currently_taking: boolean };
	pgx: { gene: string; diplotype: string | null; phenotype: string | null; evidence: string; call_state: CallState; src: string }[];
	phenoconversion: { gene: string; by: string; inhibitor_strength: string; computed: boolean; functional_phenotype: string | null; src: string }[];
	current_medications_relevant: { name: string; since: IsoDate | null; src: string }[];
	relevant_labs: { name: string; value: string; unit: string; date: IsoDate; src: string }[];
	guidance: { source: string; strength: string; applies_to: string; quote: string; url: string }[];
	not_assessed: string[];
	limitations: string[];
	notice: string;
}

// ---------------------------------------------------- lane C: wearables, records ----

export interface WearableMetric {
	metric: string;
	label: string;
	unit: string;
	latest: number | null;
	avg_30d: number | null;
	series: { date: IsoDate; value: number | null; min: number | null; max: number | null }[];
	coverage_pct: number;
	source: string;
}

export interface WearablesSummary {
	metrics: WearableMetric[];
	devices: { source_id: number; provider: string; device: string | null; last_sample: IsoDateTime | null; coverage_pct: number }[];
}

export interface RecordOut {
	id: Uuid;
	kind: 'condition' | 'medication' | 'allergy' | 'immunization' | 'procedure';
	title: string;
	detail: Record<string, unknown>;
	status: Lifecycle;
	src: string;
	recorded_at: IsoDateTime;
}

// ---------------------------------------------------------- lane H: timeline ----

export interface TimelineEvent {
	date: IsoDate;
	type: 'lab' | 'visit' | 'imaging' | 'diagnosis' | 'medication' | 'vaccine' | 'genome' | 'document';
	title: string;
	detail: string | null;
	country: string | null;
	flags: string[];
	src: string | null;
}

// ------------------------------------------------ lane B: documents ----

export interface DocumentSummary {
	id: Uuid;
	profile_id: Uuid | null;
	filename: UntrustedText | null;
	doc_type: string | null;
	provider_name: string | null;
	collected_at: IsoDateTime | null;
	pages: number | null;
	status: 'active' | 'retracted';
	flags: Record<string, unknown>;
	open_review: number;
	snippet: UntrustedText | null; // full-text search hit, UI only
}

export interface DocumentDetail extends DocumentSummary {
	mime: string;
	size_bytes: number;
	pages_info: { page_no: number; width_pt: number; height_pt: number; text_layer_ok: boolean; flags: Record<string, unknown> }[];
}

// ---------------------------------------- lane G: tokens, audit, family ----

export interface TokenOut {
	id: Uuid;
	label: string;
	prefix: string;
	scopes: Scope[];
	profiles: string[]; // slugs
	default_profile: string | null;
	identity_mode: 'full' | 'pseudonymous' | 'minimal';
	lang: 'es' | 'en';
	units: 'si' | 'conventional' | 'as_reported';
	limits: Record<string, number>;
	created_at: IsoDateTime;
	expires_at: IsoDateTime;
	last_used_at: IsoDateTime | null;
	revoked_at: IsoDateTime | null;
}

export interface AuditOut {
	id: number;
	ts: IsoDateTime;
	actor: string; // 'Alex Rivera' or 'Claude Code · laptop (token)'
	via: string;
	action: string;
	tool: string | null;
	profile: string | null;
	rows_out: number | null;
	bytes_out: number | null;
	result_kind: string | null;
}

export interface TokenUsage {
	token_id: Uuid;
	day: IsoDate;
	facts: number;
	bytes: number;
	variant_lookups: number;
	proposals: number;
	limits: Record<string, number>;
}

export interface Household {
	members: { user_id: Uuid; display_name: string }[];
	profiles: ProfileSummary[];
	grants: { user_id: Uuid; profile: string; role: Role; categories: Scope[]; expires_at: IsoDateTime | null }[];
}
