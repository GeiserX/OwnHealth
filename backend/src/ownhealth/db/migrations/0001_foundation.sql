-- 0001 foundation: the full v0.1 schema (docs/design/architecture.md section 6.2).
-- Forward-only. Lanes add their own numbered files in their reserved ranges (CONTRACTS.md).
-- Names differ from the DDL sketch in two places to avoid reserved words:
--   "grant" -> profile_grant, "token" -> api_token. "page" -> document_page.

-- ---------------------------------------------------------------- helpers ----

-- Clinical rows are immutable. The only update allowed changes the lifecycle:
-- provisional -> current | superseded | retracted, current -> superseded | retracted,
-- together with status_reason and superseded_at. Every other column must stay equal.
create function oh_clinical_lifecycle_guard() returns trigger language plpgsql as $$
declare
  frozen text[] := array['status', 'status_reason', 'superseded_at'];
begin
  if (to_jsonb(new) - frozen) is distinct from (to_jsonb(old) - frozen) then
    raise exception 'clinical rows are immutable: insert a new version instead (table %)', tg_table_name
      using errcode = 'check_violation';
  end if;
  if new.status is distinct from old.status then
    if not (
      (old.status = 'provisional' and new.status in ('current', 'superseded', 'retracted'))
      or (old.status = 'current' and new.status in ('superseded', 'retracted'))
    ) then
      raise exception 'lifecycle transition % -> % not allowed (table %)', old.status, new.status, tg_table_name
        using errcode = 'check_violation';
    end if;
  elsif new.superseded_at is distinct from old.superseded_at
        or new.status_reason is distinct from old.status_reason then
    raise exception 'status_reason and superseded_at change only with the lifecycle (table %)', tg_table_name
      using errcode = 'check_violation';
  end if;
  return new;
end $$;

-- The audit log is append-only, whatever role the app connects as.
create function oh_append_only() returns trigger language plpgsql as $$
begin
  raise exception '% is append-only', tg_table_name using errcode = 'insufficient_privilege';
end $$;

-- ------------------------------------------------- people, logins, access ----

create table household (
  id uuid primary key default gen_random_uuid(),
  name text not null,
  created_at timestamptz not null default now());

create table household_setting (    -- age of majority, timezone, ...
  household_id uuid not null references household on delete cascade,
  key text not null,
  value jsonb not null,
  primary key (household_id, key));

create table app_user (              -- a login; a child or a deceased relative has none
  id uuid primary key default gen_random_uuid(),
  household_id uuid not null references household,
  display_name text not null,
  locale text not null default 'es' check (locale in ('es', 'en')),
  oidc_subject text unique,
  created_at timestamptz not null default now(),
  disabled_at timestamptz);

create table passkey (
  id bytea primary key,              -- WebAuthn credential id
  user_id uuid not null references app_user on delete cascade,
  public_key bytea not null,
  sign_count bigint not null default 0,
  transports text[] not null default '{}',
  label text,
  created_at timestamptz not null default now(),
  last_used_at timestamptz);

create table recovery_code (         -- 10 single-use codes printed at enrolment
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references app_user on delete cascade,
  code_sha256 bytea not null unique,
  created_at timestamptz not null default now(),
  used_at timestamptz);

create table user_session (          -- browser session; the cookie holds the secret, we keep its hash
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references app_user on delete cascade,
  secret_sha256 bytea not null unique,
  created_at timestamptz not null default now(),
  last_seen_at timestamptz not null default now(),
  expires_at timestamptz not null,   -- absolute limit (12 h); idle limit checked against last_seen_at
  step_up_at timestamptz,            -- last fresh passkey assertion, for step-up actions
  ip inet, user_agent text,
  revoked_at timestamptz);

create table profile (               -- a person whose data we hold
  id uuid primary key default gen_random_uuid(),
  household_id uuid not null references household,
  user_id uuid unique references app_user,          -- set when the person has claimed the profile
  slug text not null check (slug ~ '^[a-z0-9][a-z0-9-]{0,39}$' and slug <> 'me'),
  display_name text not null,
  relationship text,                                 -- self | partner | child | parent | other
  birth_date date,
  sex_at_birth text check (sex_at_birth in ('female', 'male', 'unknown')),
  country text,                                      -- ISO 3166-1 alpha-2, guides screening rules
  deceased boolean not null default false,
  unit_pref text not null default 'si' check (unit_pref in ('si', 'conventional', 'as_reported')),
  -- two separate consents for sensitive genome findings; seeing is not consent to hand it to an agent
  genome_sensitive_ui boolean not null default false,
  genome_sensitive_agents boolean not null default false,
  -- categories the profile's owner switched off for everyone else
  locks text[] not null default '{}',
  created_at timestamptz not null default now(),
  unique (household_id, slug));

create table profile_grant (         -- "grant" in the docs: one row per user and profile
  user_id uuid not null references app_user on delete cascade,
  profile_id uuid not null references profile on delete cascade,
  role text not null check (role in ('owner', 'guardian', 'caregiver', 'viewer')),
  categories text[] not null,        -- scope names: the most this user may read here
  break_glass boolean not null default false,
  expires_at timestamptz,
  granted_by uuid references app_user,
  granted_at timestamptz not null default now(),
  primary key (user_id, profile_id),
  check (not break_glass or expires_at is not null));
create index on profile_grant (profile_id);

create table api_token (             -- "token" in the docs: MCP personal access token
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references app_user on delete cascade,
  label text not null,
  prefix text not null unique,       -- 'oh_pat_3fA9xk2Q': the lookup key, shown in the UI
  secret_sha256 bytea not null unique,
  scopes text[] not null,
  profile_ids uuid[] not null,       -- never an implicit "all profiles"
  default_profile uuid references profile,
  identity_mode text not null default 'full' check (identity_mode in ('full', 'pseudonymous', 'minimal')),
  lang text not null default 'es' check (lang in ('es', 'en')),
  units text not null default 'si' check (units in ('si', 'conventional', 'as_reported')),
  rate_per_min int not null default 60,
  egress_facts_per_day int not null default 2000,
  egress_bytes_per_day bigint not null default 2000000,
  genome_raw_lookups_per_day int not null default 200,
  document_pages_per_day int not null default 300,
  proposals_per_day int not null default 50,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  last_used_at timestamptz,
  revoked_at timestamptz);

-- -------------------------------------------------- documents and extraction ----

create table document (
  id uuid primary key default gen_random_uuid(),
  household_id uuid not null references household,
  profile_id uuid references profile,                -- null until assigned in review
  blob_sha256 bytea not null,
  filename text,                                     -- as uploaded; untrusted text
  mime text not null,
  size_bytes bigint not null,
  pages int,
  doc_type text check (doc_type in ('lab_report', 'imaging_report', 'discharge', 'prescription',
        'genomics', 'ecg', 'other')),
  language text,
  provider_fingerprint text, provider_name text,
  report_status text check (report_status in ('preliminary', 'final', 'amended')),
  collected_at timestamptz, reported_at timestamptz,
  supersedes_document uuid references document,
  identity_name_hash bytea, identity_birth_date date,
  identity_match text check (identity_match in ('matched', 'hinted', 'mismatch', 'missing')),
  status text not null default 'active' check (status in ('active', 'retracted')),
  status_reason text,
  flags jsonb not null default '{}',                 -- hidden_text, possible_injection, ...
  created_at timestamptz not null default now());
create unique index document_profile_blob on document (coalesce(profile_id, household_id), blob_sha256);
create index on document (profile_id, collected_at desc);

create table document_source (      -- one row per arrival; the same bytes may arrive twice
  document_id uuid not null references document on delete cascade,
  channel text not null check (channel in ('upload', 'fhir', 'apple_health', 'email', 'folder',
        'paperless', 'manual', 'genome', 'demo')),
  source_ref text,                   -- message-id, paperless id; never content
  received_at timestamptz not null default now());

create table document_page (
  document_id uuid not null references document on delete cascade,
  page_no int not null check (page_no >= 1),
  width_pt real not null,
  height_pt real not null,
  text_layer_ok boolean not null,
  phash bytea,
  words_blob bytea,                  -- sha256 of the word layer JSON (words + bboxes + fonts)
  image_blob bytea,                  -- sha256 of the rendered page PNG
  flags jsonb not null default '{}',
  text_content text,                 -- page text; returned only under read:documents_text or in the UI
  fts tsvector,
  primary key (document_id, page_no));
create index document_page_fts on document_page using gin (fts);

create table extraction_run (
  id uuid primary key default gen_random_uuid(),
  document_id uuid not null references document on delete cascade,
  engine text, engine_version text,
  extractor text not null, extractor_version text not null,  -- 'template:es/labnorte@1', 'rules', 'agent'
  config_hash bytea not null,
  status text not null default 'running' check (status in ('running', 'done', 'failed')),
  error text,
  started_at timestamptz not null default now(),
  finished_at timestamptz);

create table candidate_row (
  id uuid primary key default gen_random_uuid(),
  extraction_run_id uuid not null references extraction_run on delete cascade,
  page_no int not null,
  bbox real[] not null check (array_length(bbox, 1) = 4),   -- PDF points, origin top-left
  cells jsonb not null,              -- name, value, unit, ref_range, flag: {text, bbox, conf}
  row_conf real not null check (row_conf between 0 and 1),
  reasons text[] not null default '{}',
  state text not null default 'candidate'
    check (state in ('candidate', 'auto_accepted', 'in_review', 'confirmed', 'rejected')),
  observation_id uuid);              -- FK added after observation exists

create table expected_test (         -- "pendiente", "folgt", "pending" lines on a report
  report_document_id uuid not null references document on delete cascade,
  printed_name text not null,
  alias_group text,
  reason text);

-- ---------------------------------------------------------------- provenance ----

create table src (
  id text primary key check (id ~ '^src_[a-z0-9_]+$'),     -- the citation id shown over MCP
  profile_id uuid references profile,
  kind text not null check (kind in ('document_region', 'fhir_import', 'apple_health',
        'genome_facts', 'manual', 'device', 'agent_proposal', 'calculated')),
  document_id uuid references document,
  page_no int,
  bbox real[] check (bbox is null or array_length(bbox, 1) = 4),
  extraction_run_id uuid references extraction_run,
  candidate_row_id uuid references candidate_row,
  import_ref text,                   -- FHIR resource id, facts file path#finding id, export file
  label text,                        -- short display: 'LabNorte · 2026-09-14 · p2'
  actor_kind text not null check (actor_kind in ('user', 'token', 'system')),
  actor_id uuid,
  created_at timestamptz not null default now(),
  check (kind <> 'document_region' or (document_id is not null and page_no is not null)));

-- ----------------------------------------------------------- clinical facts ----
-- Common columns on every clinical table: profile_id, status, status_reason, src_id,
-- supersedes, superseded_at, sensitivity, effective_at, recorded_at, fhir.

create table observation (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  category text not null check (category in ('laboratory', 'vital-signs', 'survey', 'exam', 'activity')),
  document_id uuid references document,
  candidate_row_id uuid references candidate_row,
  source_kind text not null check (source_kind in ('clinical', 'consumer', 'device', 'self_reported',
        'agent_written')),
  -- truth layer: what the source says, never changed
  printed_name text not null,
  printed_value text not null,
  printed_unit text,
  printed_ref_range text,
  printed_flag text,
  -- parsed
  value_num numeric,
  value_cmp text check (value_cmp in ('<', '<=', '>', '>=')),
  value_text text, value_code text,
  method text,
  ref_low numeric, ref_high numeric, ref_applies jsonb,
  specimen text,
  -- derived, recomputable
  loinc text,
  alias_group text not null,         -- series key even when uncoded
  unit_ucum text,
  value_canonical numeric,
  unit_canonical text,
  interpretation text check (interpretation in ('L', 'N', 'H', 'LL', 'HH', 'A')),
  derivation_version text not null,
  -- time and lifecycle
  effective_at timestamptz not null,
  recorded_at timestamptz not null default now(),
  src_id text not null references src,
  report_key text,                   -- groups one draw
  accepted_by text not null check (accepted_by in ('auto', 'person', 'import')),
  reviewed_by uuid references app_user,
  reviewed_at timestamptz,
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references observation,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']),
  fhir jsonb not null);
create index on observation (profile_id, alias_group, effective_at desc) where superseded_at is null;
create index on observation (profile_id, loinc, effective_at desc) where loinc is not null;
create index on observation (src_id);
create unique index observation_one_successor on observation (supersedes) where supersedes is not null;
create trigger observation_lifecycle before update on observation
  for each row execute function oh_clinical_lifecycle_guard();

alter table candidate_row add foreign key (observation_id) references observation;

create table condition (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  text text not null,
  code_system text, code text,
  clinical_status text not null default 'active',     -- active | recurrence | relapse | inactive | remission | resolved
  verification_status text not null default 'provisional'
    check (verification_status in ('provisional', 'confirmed', 'refuted', 'unconfirmed', 'differential',
          'entered-in-error')),
  onset date, abatement date,
  effective_at timestamptz,
  recorded_at timestamptz not null default now(),
  src_id text not null references src,
  accepted_by text not null default 'person' check (accepted_by in ('auto', 'person', 'import')),
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references condition,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']),
  fhir jsonb not null);

create table medication_statement (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  kind text not null default 'medication' check (kind in ('medication', 'supplement')),
  name text not null, ingredient text, strength text, dose_text text,
  code_system text, code text,       -- RxNorm or national code as received
  started date, stopped date,
  med_status text not null default 'active' check (med_status in ('active', 'stopped', 'on_hold')),
  reason_text text,
  effective_at timestamptz,
  recorded_at timestamptz not null default now(),
  src_id text not null references src,
  accepted_by text not null default 'person' check (accepted_by in ('auto', 'person', 'import')),
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references medication_statement,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']),
  fhir jsonb not null);

create table allergy (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  substance text not null,
  code_system text, code text,
  criticality text check (criticality in ('low', 'high', 'unable-to-assess')),
  reaction_text text,
  clinical_status text not null default 'active',
  verification_status text not null default 'confirmed',
  effective_at timestamptz,
  recorded_at timestamptz not null default now(),
  src_id text not null references src,
  accepted_by text not null default 'person' check (accepted_by in ('auto', 'person', 'import')),
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references allergy,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']),
  fhir jsonb not null);

create table immunization (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  vaccine text not null,
  code_system text, code text,       -- CVX or a national code as received
  occurred date,
  dose_number int,
  lot text,
  effective_at timestamptz,
  recorded_at timestamptz not null default now(),
  src_id text not null references src,
  accepted_by text not null default 'person' check (accepted_by in ('auto', 'person', 'import')),
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references immunization,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']),
  fhir jsonb not null);

create table procedure (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  text text not null,
  code_system text, code text,
  performed date,
  effective_at timestamptz,
  recorded_at timestamptz not null default now(),
  src_id text not null references src,
  accepted_by text not null default 'person' check (accepted_by in ('auto', 'person', 'import')),
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references procedure,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']),
  fhir jsonb not null);

create table encounter (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  class text, provider_name text,
  period_start timestamptz, period_end timestamptz,
  reason_text text,
  effective_at timestamptz,
  recorded_at timestamptz not null default now(),
  src_id text not null references src,
  accepted_by text not null default 'import' check (accepted_by in ('auto', 'person', 'import')),
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references encounter,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']),
  fhir jsonb not null);

create table imaging_study (         -- reference only in v0.1; pixels live in Orthanc (v1.0)
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  study_instance_uid text not null unique,
  orthanc_id text, modality text, study_date date, description text,
  report_document_id uuid references document,
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  src_id text not null references src,
  recorded_at timestamptz not null default now(),
  fhir jsonb not null);

do $$
declare t text;
begin
  foreach t in array array['condition', 'medication_statement', 'allergy', 'immunization', 'procedure',
                           'encounter'] loop
    execute format('create index on %I (profile_id, status)', t);
    execute format('create unique index %I on %I (supersedes) where supersedes is not null',
                   t || '_one_successor', t);
    execute format('create trigger %I before update on %I for each row execute function oh_clinical_lifecycle_guard()',
                   t || '_lifecycle', t);
  end loop;
end $$;

-- ------------------------------------------------------------------- genome ----

create table genome_dataset (        -- one import of an OwnGenome genome-facts.json
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  sample_id text not null,
  assay text not null check (assay in ('wgs', 'wes', 'long_read', 'vcf_only', 'chip', 'chip_imputed')),
  facts_schema text not null,        -- 'ownhealth.genome-facts/v1'
  facts_sha256 bytea not null unique,
  facts_blob bytea not null,         -- the facts file itself, in the blob store
  vcf_blob bytea, tbi_blob bytea,    -- tabix-indexed VCF and index, copied after a sha256 check
  coverage_blob bytea,
  build text not null default 'GRCh38',
  owngenome_version text not null,
  db_versions jsonb not null,
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  src_id text not null references src,
  imported_at timestamptz not null default now());

create table genome_finding (
  id uuid primary key default gen_random_uuid(),
  dataset_id uuid not null references genome_dataset,
  profile_id uuid not null references profile,
  fact_id text not null check (fact_id ~ '^f_[0-9a-f]{16}$'),   -- deterministic id from the facts file
  category text not null check (category in ('pgx', 'monogenic_pathogenic', 'acmg_sf', 'carrier', 'hla',
        'trait', 'risk_marker', 'prs', 'ancestry')),
  gene text, rsid text, hgvs text, vrs_id text, clinvar_variation_id bigint,
  diplotype text, phenotype text, activity_score numeric,
  zygosity text,
  evidence text not null check (evidence in ('E1', 'E2', 'E3', 'E4')),
  confidence text not null check (confidence in ('high', 'medium', 'low', 'unconfirmed')),
  call_state text check (call_state in ('ref', 'alt', 'no_call', 'not_on_array', 'not_covered')),
  verification_status text not null default 'provisional'
    check (verification_status in ('provisional', 'confirmed', 'refuted')),
  sensitive boolean not null,
  adult_onset boolean not null,
  carrier boolean not null default false,
  recorded_at timestamptz not null default now(),
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  src_id text not null references src,
  supersedes uuid references genome_finding,
  superseded_at timestamptz,
  fact jsonb not null);              -- the facts-file record, verbatim
create index on genome_finding (profile_id, category, gene);
create index on genome_finding (profile_id, fact_id);
create unique index genome_finding_one_successor on genome_finding (supersedes) where supersedes is not null;
create trigger genome_finding_lifecycle before update on genome_finding
  for each row execute function oh_clinical_lifecycle_guard();

create table genome_coverage (
  dataset_id uuid not null references genome_dataset on delete cascade,
  scope text not null check (scope in ('gene', 'locus', 'panel')),
  target text not null,              -- gene symbol, locus id or panel id
  panel text,
  state text not null check (state in ('assessed', 'partial', 'not_assessed')),
  reason text,
  fraction_callable real,
  note text,
  primary key (dataset_id, scope, target));

-- ---------------------------------------------------------------- wearables ----

create table wearable_source (
  id smallint generated always as identity primary key,
  profile_id uuid not null references profile,
  provider text not null,            -- apple_health | oura | garmin | manual ...
  device text,                       -- 'Apple Watch', 'iPhone'
  external_id text);
create unique index wearable_source_key on wearable_source (profile_id, provider, coalesce(device, ''),
  coalesce(external_id, ''));

create table wearable_sample (       -- narrow on purpose; no surrogate key
  profile_id uuid not null references profile,
  metric text not null,              -- 'hr', 'hrv_rmssd', 'steps', 'sleep_asleep', ...
  t timestamptz not null,
  t_end timestamptz,
  v real not null,
  tz_offset_min smallint,            -- original UTC offset of the source record
  source_id smallint not null references wearable_source,
  unique (profile_id, metric, source_id, t));          -- re-import is idempotent

create table wearable_rollup (
  profile_id uuid not null references profile,
  metric text not null,
  source_id smallint not null references wearable_source,
  bucket text not null check (bucket in ('hour', 'day')),
  t timestamptz not null,
  n int not null,
  v_min real, v_max real, v_avg real, v_sum real,
  v_p10 real, v_p50 real, v_p90 real,
  coverage real not null check (coverage between 0 and 1),
  primary key (profile_id, metric, bucket, t, source_id));

create table source_priority (       -- applied at query time
  profile_id uuid not null references profile,
  metric text not null,
  source_ids smallint[] not null,
  primary key (profile_id, metric));

-- -------------------------------------------------- narrative, review, audit ----

create table note (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  kind text not null check (kind in ('priorities', 'journal', 'finding_comment')),
  author_kind text not null check (author_kind in ('person', 'agent', 'import')),
  author_id uuid,
  lang text,
  attach_to text,                    -- a src id or 'status'
  body text not null check (length(body) <= 4000),
  effective_at timestamptz,
  recorded_at timestamptz not null default now(),
  src_id text references src,
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references note,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']));
create trigger note_lifecycle before update on note
  for each row execute function oh_clinical_lifecycle_guard();

create table review_item (
  id uuid primary key default gen_random_uuid(),
  household_id uuid references household,         -- null only for system items (a failed blob.gc)
  profile_id uuid references profile,
  kind text not null check (kind in ('candidate_row', 'agent_proposal', 'fhir_unmapped',
        'document_identity', 'amended_report', 'rederive_diff', 'genome_reinterpretation',
        'job_failed', 'restore_check_failed')),
  reasons text[] not null default '{}',
  document_id uuid references document,
  candidate_row_id uuid references candidate_row,
  observation_id uuid references observation,
  ref_table text, ref_id uuid,
  payload jsonb not null,            -- what was read, the checks and their results
  row_conf real,
  proposed_by_token uuid references api_token,
  context_documents uuid[],
  context_src_ids text[],
  status text not null default 'open' check (status in ('open', 'accepted', 'rejected', 'dismissed')),
  decided_by uuid references app_user,
  decided_at timestamptz,
  decision_reason text,
  result_ids uuid[],                 -- rows created when accepted
  created_at timestamptz not null default now());
create index on review_item (household_id, status, created_at);
create index on review_item (profile_id, status);
create index on review_item (document_id);

create table audit_event (           -- append-only: a trigger rejects UPDATE, DELETE and TRUNCATE
  id bigint generated always as identity primary key,
  ts timestamptz not null default now(),
  actor_kind text not null check (actor_kind in ('user', 'token', 'share', 'system')),
  actor_id uuid,
  token_id uuid,
  client text,                       -- token label or client id
  via text not null check (via in ('ui', 'api', 'mcp', 'job', 'share', 'export')),
  action text not null,              -- read | search | propose | export | grant_change | login | purge ...
  tool text,                         -- MCP tool or REST route
  profile_id uuid,
  resource_type text,
  resource_ids text[],
  src_ids text[],
  args_summary text,                 -- codes and ids only; free text hashed
  result_kind text,                  -- ok | denied | error | budget_cut
  rows_out int,
  bytes_out int,
  withheld jsonb,
  protocol_version text,
  ip inet,
  user_agent text);
create index on audit_event (profile_id, ts desc);
create index on audit_event (actor_id, ts desc);
create trigger audit_event_no_update before update or delete on audit_event
  for each row execute function oh_append_only();
create trigger audit_event_no_truncate before truncate on audit_event
  for each statement execute function oh_append_only();

create table state_snapshot (
  profile_id uuid not null references profile,
  as_of timestamptz not null,
  rules_version text not null,
  rules_sha256 bytea not null,
  derivation_version text not null,
  state_hash bytea not null,
  body jsonb not null,
  inputs_purged boolean not null default false,
  created_at timestamptz not null default now(),
  primary key (profile_id, as_of, rules_sha256));

-- ---------------------------------------------------------------- job queue ----

create table job (
  id bigint generated always as identity primary key,
  queue text not null default 'core' check (queue in ('core', 'ocr')),
  kind text not null,                -- 'import.pdf', 'state.rebuild', ...
  payload jsonb not null,            -- ids only, never document content
  dedupe_key text,
  priority smallint not null default 5,
  state text not null default 'queued' check (state in ('queued', 'running', 'done', 'failed', 'dead')),
  run_after timestamptz not null default now(),
  attempts int not null default 0,
  max_attempts int not null default 5,
  locked_by text,
  locked_until timestamptz,
  last_error text,                   -- exception class and message, no payload content
  result jsonb,
  created_at timestamptz not null default now(),
  finished_at timestamptz);
create unique index job_dedupe on job (dedupe_key) where state in ('queued', 'running');
create index job_ready on job (queue, priority, id) where state = 'queued';

-- ------------------------------------------------------------ terminology ----

create table analyte_alias (         -- user-added aliases; shipped ones live in data/terminology
  id uuid primary key default gen_random_uuid(),
  household_id uuid references household,
  alias text not null,
  alias_norm text not null,          -- lower case, accents and punctuation folded
  lang text,
  alias_group text not null,
  loinc text,
  unit text,
  specimen text,
  added_by uuid references app_user,
  created_at timestamptz not null default now());
create unique index analyte_alias_key on analyte_alias (coalesce(household_id::text, ''), alias_norm,
  coalesce(lang, ''), coalesce(unit, ''), coalesce(specimen, ''));
