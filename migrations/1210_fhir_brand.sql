-- 1210, v1.0 Connected (records block 1210-1219): the US endpoint directory a person searches by
-- organisation name instead of pasting a FHIR base URL (connectors.md section 7, "Endpoint discovery").
--
-- The rows come from each vendor's own Brands or endpoint file, fetched by the household's `brands`
-- source (ownhealth-ingest-structured src/brands.rs). That source exists only after a person first
-- opens the US connection wizard, so an install that never connects a US portal holds no row here and
-- sends no request to a US vendor. The vendor files are never kept: each entry is cut to
-- (name, endpoint, state). Not health data and not a clinical row: no profile_id, no lifecycle trigger.
-- Removing the source removes its directory.

-- The last good copy of each vendor's file: when it was fetched and how many entries it gave. A run
-- whose file fails to download, to parse or to reach the vendor's minimum leaves this row and the
-- vendor's entries as they were, so the person keeps searching the old copy; `fetched_at` then ages,
-- and the directory reads stale after three missed weekly runs.
create table fhir_brand_feed (
  source_id uuid not null references source on delete cascade,
  vendor text not null check (vendor ~ '^[a-z0-9_]{1,40}$'),
  fetched_at timestamptz not null,
  entries integer not null check (entries >= 0),
  primary key (source_id, vendor));

-- One organisation or facility and the FHIR base URL it answers on. A facility linked to its parent
-- by partOf carries the parent's endpoint. A vendor's entries are replaced together, in one
-- transaction with its feed row.
create table fhir_brand (
  id bigint generated always as identity primary key,
  source_id uuid not null,
  vendor text not null,
  name text not null check (char_length(name) between 1 and 200),
  endpoint text not null check (endpoint ~ '^https://' and char_length(endpoint) <= 2048),
  state text check (char_length(state) between 1 and 40),
  foreign key (source_id, vendor) references fhir_brand_feed on delete cascade);
create index on fhir_brand (source_id, vendor);
