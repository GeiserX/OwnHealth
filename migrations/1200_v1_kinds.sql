-- 1200, v1.0 Connected foundation: every kind, purpose and channel v1.0 needs, connector clients and
-- the connection state the Connections screen reads.
--
-- Each CHECK below is re-created with the full union of every value shipped so far (CONTRACTS.md
-- section 7). Each list has one Rust constant, and server/crates/ownhealth/tests/check_mirror.rs fails
-- when the two differ. In v1.0 only this file changes these six lists.

alter table source drop constraint source_kind_check;
alter table source add constraint source_kind_check check (kind in ('email', 'folder', 'paperless', 'share',
  'shortcut', 'health_auto_export', 'gadgetbridge', 'nightscout', 'clinvar', 'smart', 'bluebutton', 'brands',
  'openwearables', 'dexcom'));

alter table secret drop constraint secret_purpose_check;
alter table secret add constraint secret_purpose_check check (purpose in ('imap_password', 'paperless_token',
  'nightscout_token', 'pdf_password', 'connector_refresh_token', 'connector_client_secret',
  'openwearables_api_key'));

alter table auth_challenge drop constraint auth_challenge_purpose_check;
alter table auth_challenge add constraint auth_challenge_purpose_check check (purpose in ('setup', 'register',
  'login', 'step_up', 'enroll', 'invite', 'oidc', 'connector'));

alter table document_source drop constraint document_source_channel_check;
alter table document_source add constraint document_source_channel_check check (channel in ('upload',
  'fhir', 'apple_health', 'email', 'folder', 'paperless', 'manual', 'genome', 'demo', 'share', 'shortcut',
  'health_auto_export', 'gadgetbridge', 'nightscout', 'smart', 'bluebutton', 'shl', 'shc', 'ccda',
  'openwearables', 'dexcom', 'imaging'));

alter table src drop constraint src_kind_check;
alter table src add constraint src_kind_check check (kind in ('document_region', 'fhir_import', 'apple_health',
  'genome_facts', 'manual', 'device', 'agent_proposal', 'calculated', 'dicom'));

alter table review_item drop constraint review_item_kind_check;
alter table review_item add constraint review_item_kind_check check (kind in ('candidate_row', 'agent_proposal',
  'fhir_unmapped', 'document_identity', 'amended_report', 'rederive_diff', 'genome_reinterpretation', 'job_failed',
  'restore_check_failed', 'pending_document', 'near_duplicate', 'encrypted_document', 'source_gap',
  'fhir_unmapped_summary', 'family_proposal'));

-- ------------------------------------------------------------ connector clients ----
-- The app a household registered with one vendor (SMART, Blue Button, Dexcom): one client serves every
-- member's source of that vendor. issuer is the authorization server the client belongs to, or null when
-- one client id works with every server of the vendor. A public client has no secret. Each member's
-- refresh token stays in source.secret_id.
create table connector_client (
  id uuid primary key default gen_random_uuid(),
  household_id uuid not null references household on delete cascade,
  vendor text not null,
  issuer text,
  client_id text not null,
  client_secret_id uuid references secret on delete restrict,
  redirect_uri text not null,
  created_at timestamptz not null default now(),
  unique (household_id, vendor));

-- A client's secret goes with the client.
create function connector_client_drop_secret() returns trigger language plpgsql as $$
begin
  delete from secret where id = old.client_secret_id;
  return null;
end $$;
create trigger connector_client_drop_secret after delete on connector_client
  for each row when (old.client_secret_id is not null) execute function connector_client_drop_secret();

-- --------------------------------------------------------- connection state ----
-- reconsent_needed_at: the vendor refused the token; the person must connect again (reconnect clears it).
-- grant_expires_at: when the person's consent at the vendor ends. access_expires_at: when the current
-- access token ends.
alter table source
  add column connector_client_id uuid references connector_client on delete restrict,
  add column reconsent_needed_at timestamptz,
  add column grant_expires_at timestamptz,
  add column access_expires_at timestamptz;
