-- 1230, v1.0 Connected, imaging: DICOM studies from hospital CDs and zips (slice 41).
--
-- An upload (a zip, or a CD folder the app zips) is kept as one zip in the profile's blob store before
-- anything reads it, so Orthanc can be rebuilt from these zips; zip_blob is registered for profile
-- delete (ownhealth_auth::household::BLOB_COLUMNS). The same zip uploaded twice for one profile is one
-- row. Each study the import records names the upload it came from.
create table imaging_import (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  zip_blob bytea not null,
  filename text,                     -- as uploaded; untrusted text
  size_bytes bigint not null,
  files int not null,                -- files in the zip, folders left out
  created_by uuid references app_user on delete set null,
  created_at timestamptz not null default now(),
  unique (profile_id, zip_blob));

alter table imaging_study add column import_id uuid references imaging_import on delete set null;
create index on imaging_study (import_id);

-- A study the import did not send to Orthanc waits as an `imaging_study` review item: a patient who
-- is not the profile's, a second patient on the disc, a StudyInstanceUID that another profile or
-- another patient holds already. The full union of every kind shipped so far (CONTRACTS.md section 7).
alter table review_item drop constraint review_item_kind_check;
alter table review_item add constraint review_item_kind_check check (kind in ('candidate_row', 'agent_proposal',
  'fhir_unmapped', 'document_identity', 'amended_report', 'rederive_diff', 'genome_reinterpretation', 'job_failed',
  'restore_check_failed', 'pending_document', 'near_duplicate', 'encrypted_document', 'source_gap',
  'fhir_unmapped_summary', 'family_proposal', 'imaging_study'));
