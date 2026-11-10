-- 1110, v0.3 Insight: family history, one row per relative of one profile (FHIR FamilyMemberHistory).
--
-- family_member is a clinical table under the lifecycle trigger: a change is a new version that
-- supersedes the old one. The relative's conditions live inside the version (a jsonb array), so the
-- stored FamilyMemberHistory always holds them and the generic export picks them up.
--
-- The link to a relative's own profile is mutable, so it lives outside the trigger in
-- family_member_link, with ON DELETE CASCADE on both sides. A SET NULL column on family_member would be
-- an UPDATE of an immutable row: the trigger raises 23514 and deleting the linked profile fails.
-- A new member version copies its link in the same transaction.

create table family_member (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  relationship text not null,        -- HL7 v3 RoleCode family code relative to profile_id (data/family/relationships.yaml)
  display_name text check (display_name is null or length(display_name) between 1 and 80),
  sex_at_birth text check (sex_at_birth in ('female', 'male', 'unknown')),
  born_year int check (born_year between 1800 and 2200),
  deceased boolean not null default false,
  death_age int check (death_age between 0 and 130),
  -- [{kind, text, code_system, code, onset_age, onset_approx}], kind from data/family/conditions.yaml
  conditions jsonb not null default '[]' check (jsonb_typeof(conditions) = 'array'),
  effective_at timestamptz,
  recorded_at timestamptz not null default now(),
  src_id text not null references src,
  accepted_by text not null default 'person' check (accepted_by in ('auto', 'person', 'import')),
  status text not null default 'current' check (status in ('provisional', 'current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references family_member,
  superseded_at timestamptz,
  sensitivity text[] not null default '{}'
    check (sensitivity <@ array['mental_health', 'reproductive', 'sexual_health']),
  fhir jsonb not null,
  check (death_age is null or deceased));
create index on family_member (profile_id, status);
create index on family_member (src_id);
create unique index family_member_one_successor on family_member (supersedes) where supersedes is not null;
create trigger family_member_lifecycle before update on family_member
  for each row execute function oh_clinical_lifecycle_guard();

create table family_member_link (
  family_member_id uuid primary key references family_member on delete cascade,
  linked_profile_id uuid not null references profile on delete cascade);
create index on family_member_link (linked_profile_id);
