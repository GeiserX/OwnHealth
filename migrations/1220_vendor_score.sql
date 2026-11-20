-- 1220, v1.0 Connected (wearables block 1220-1229): vendor scores, stored apart from measurements.
--
-- Readiness, recovery, sleep and activity scores, stress, strain and Body Battery are numbers a
-- vendor computes with its own method (connectors.md section 4). They are not measurements, so they never
-- go into wearable_sample: they live here, tagged computed_by 'provider', with the provider's name.
-- The rule pack, the calculators and MCP never read this table; only the Wearables page shows it.
--
-- One row per profile, provider, category and time, so a re-pull updates the row in place.
-- `qualifier` is the vendor's own word for the score ("Optimal"), `components` its contributors.
-- Not a clinical row: no lifecycle trigger. Deleting the profile deletes its scores.
create table vendor_score (
  id bigint generated always as identity primary key,
  profile_id uuid not null references profile,
  source_id uuid references source on delete set null,
  provider text not null check (provider ~ '^[a-z0-9_]{1,40}$'),
  category text not null check (category in ('readiness', 'recovery', 'sleep', 'activity', 'stress',
    'strain', 'body_battery')),
  t timestamptz not null,
  value double precision not null check (value = value and value between -1e9 and 1e9),
  qualifier text check (char_length(qualifier) <= 80),
  components jsonb check (jsonb_typeof(components) = 'object'),
  computed_by text not null default 'provider' check (computed_by = 'provider'),
  unique (profile_id, provider, category, t));

-- The guard: a series key that names a vendor score cannot reach wearable_sample. A key is refused
-- when one of its words (split on '_') is readiness, battery, stress, strain or score, or when its
-- first word is recovery ("hr_recovery" is a measurement). ownhealth-devices scores::is_score_key is
-- the same rule in Rust, and its tests check that both agree.
alter table wearable_sample add constraint wearable_sample_not_a_vendor_score check (
  metric !~ '(^|_)(readiness|battery|stress|strain|score)(_|$)' and metric !~ '^recovery(_|$)');
