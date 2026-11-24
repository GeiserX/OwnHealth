-- Journal potentially new instances before each Orthanc POST. The writer keeps the study lock.
-- A completed reply records the new instance ID. For an interrupted reply, keep the observed
-- pre-existing IDs so retry cleanup cannot remove someone else's instance of the SOP.
create table imaging_import_post (
  study_instance_uid text not null,
  import_id uuid not null references imaging_import on delete cascade,
  entry_index integer not null,
  sop_instance_uid text not null,
  preexisting_ids text[] not null,
  expected_id text,
  stored_id text,
  file_sha256 bytea not null,
  file_size bigint not null,
  primary key (study_instance_uid, import_id, entry_index));

create index on imaging_import_post (import_id);
