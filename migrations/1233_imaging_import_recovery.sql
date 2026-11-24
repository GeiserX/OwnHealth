-- A pending Orthanc write survives deletion of the import that attempted it.
alter table imaging_import_post drop constraint imaging_import_post_import_id_fkey;

-- Each retained report SOP is filed once. Its first import records the study in the same
-- transaction, after it files the reports.
create table imaging_import_report (
  study_instance_uid text not null references imaging_study (study_instance_uid) on delete cascade
    deferrable initially deferred,
  sop_instance_uid text not null,
  primary key (study_instance_uid, sop_instance_uid));
