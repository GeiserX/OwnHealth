-- Orthanc is outside the import transaction. Commit the IDs it says this upload added
-- before taking any of them back. An empty array retains the hold if another upload
-- of the same UID finished the deletes first.
create table imaging_import_cleanup (
  import_id uuid not null references imaging_import on delete cascade,
  study_instance_uid text not null,
  instance_ids text[] not null,
  primary key (study_instance_uid, import_id)
);

create index on imaging_import_cleanup (import_id);
