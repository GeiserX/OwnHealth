-- 1234, v1.0 Connected, imaging (slice 43): which upload holds which recorded study.
--
-- A study can come from more than one upload: a second disc can add a series to a recorded study, and
-- `imaging_study.import_id` names only the first. Every import that records or completes a study adds
-- a row here, in the transaction that records it. Two readers use it: the removal of an upload that
-- went to the wrong profile (allowed only when none of its studies is on the record), and the rebuild
-- of Orthanc from the stored zips after a restore, which replays every upload of the study.
create table imaging_import_study (
  import_id uuid not null references imaging_import on delete cascade,
  study_instance_uid text not null references imaging_study (study_instance_uid) on delete cascade
    deferrable initially deferred,
  primary key (study_instance_uid, import_id));

create index on imaging_import_study (import_id);

insert into imaging_import_study (import_id, study_instance_uid)
  select import_id, study_instance_uid from imaging_study where import_id is not null;

-- Whether this upload's import linked every recorded study it holds. An upload from before this table
-- (false) can hold part of a recorded study with no row above: a second disc that added a series. Its
-- removal is refused while its profile has a recorded study, and only such an upload is read for an
-- unlinked part when a rebuild comes up short. Every later upload is linked by its own import.
alter table imaging_import add column links_complete boolean not null default false;
alter table imaging_import alter column links_complete set default true;

-- A study retracted with its report document keeps the status it had (current or provisional), so
-- reactivating that document gives it back (core documents::create_document) instead of making a
-- provisional study current.
alter table imaging_study add column retracted_from text check (retracted_from in ('current', 'provisional'));
