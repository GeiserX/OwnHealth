-- 1051, v0.2: deleting a profile must never be blocked by rows that only point at its data
-- (security-privacy.md 12).
--
-- observation_link has no profile_id column, so the profile purge never visits it; a level-3 dedup
-- link to a deleted observation, src or document goes with it. A consent's default profile becomes
-- null when that profile is deleted (the purge also drops the profile from profile_ids).

alter table observation_link
  drop constraint observation_link_observation_id_fkey,
  drop constraint observation_link_src_id_fkey,
  drop constraint observation_link_document_id_fkey,
  add constraint observation_link_observation_id_fkey foreign key (observation_id) references observation on delete cascade,
  add constraint observation_link_src_id_fkey foreign key (src_id) references src on delete cascade,
  add constraint observation_link_document_id_fkey foreign key (document_id) references document on delete cascade;

alter table oauth_consent
  drop constraint oauth_consent_default_profile_fkey,
  add constraint oauth_consent_default_profile_fkey foreign key (default_profile) references profile on delete set null;
