-- 1260, v1.0 Connected, break-glass (1260-1269): a relative the owner named reaches the owner's records
-- in an emergency, after a delay the owner can cancel in (security-privacy.md section 6.5).
--
-- The owner names a contact and what the contact would see. The contact asks with a reason. The
-- request opens once its delay has passed AND the owner's calendar feed has served the request since
-- it was made (owner_feed_at), so access never opens before the owner's calendar has carried it. It
-- then stays open for 14 days, until the owner revokes it, or until it expires.
--
-- The open access lives here and never in profile_grant: the permission function adds an active
-- request to a browser session's grant (ownhealth-core access.rs), so a contact's own grant row is never
-- touched and is the same after the request ends. Tokens, OAuth consents and calendar feeds never see
-- the request.
--
-- Access control and notices, not clinical rows: no lifecycle trigger, and a person's export leaves the
-- tables out (ownhealth-ops export.rs SKIP_RAW). Deleting the profile or a login removes its rows.
create table break_glass_contact (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile on delete cascade,
  contact_user_id uuid not null references app_user on delete cascade,
  -- what an open request lets the contact read: never read:sensitive, read:genome_raw or a write
  categories text[] not null default array['read:summary', 'read:records', 'read:labs'],
  delay_hours int not null default 48 check (delay_hours between 12 and 168),
  created_by uuid references app_user on delete set null,
  created_at timestamptz not null default now(),
  unique (profile_id, contact_user_id),
  check (cardinality(categories) > 0),
  check (not categories && array['read:sensitive', 'read:genome_raw', 'write:propose']));

create table break_glass_request (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile on delete cascade,
  contact_user_id uuid not null references app_user on delete cascade,
  -- copied from the contact row when the request is made, so a later edit does not widen it
  categories text[] not null,
  -- the contact's words, for the owner; never in the audit log
  reason text not null check (char_length(reason) between 1 and 500),
  requested_at timestamptz not null default now(),
  activates_at timestamptz not null,
  -- the first time an owner calendar feed served this request: activation waits for it
  owner_feed_at timestamptz,
  status text not null default 'requested'
    check (status in ('requested', 'cancelled', 'active', 'expired', 'revoked')),
  activated_at timestamptz,
  expires_at timestamptz,
  ended_at timestamptz,
  ended_by uuid references app_user on delete set null,
  check (activates_at > requested_at),
  check (not categories && array['read:sensitive', 'read:genome_raw', 'write:propose']),
  check ((status in ('requested', 'cancelled')) = (activated_at is null)),
  check ((activated_at is null) = (expires_at is null)),
  check ((status in ('requested', 'active')) = (ended_at is null)),
  check (activated_at is null or owner_feed_at is not null));
-- one open request per contact and profile
create unique index on break_glass_request (profile_id, contact_user_id) where status in ('requested', 'active');
create index on break_glass_request (contact_user_id) where status in ('requested', 'active');
create index on break_glass_request (status, activates_at) where status in ('requested', 'active');

-- What the owner is told in the app, shown at the next login until it is read (slice 49). One row per
-- owner per step: request, cancel, activation, each use, expiry and revoke.
create table notification_outbox (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references app_user on delete cascade,
  profile_id uuid not null references profile on delete cascade,
  kind text not null check (kind in ('break_glass.request', 'break_glass.cancel', 'break_glass.activate',
                                     'break_glass.use', 'break_glass.expire', 'break_glass.revoke')),
  request_id uuid references break_glass_request on delete cascade,
  actor_id uuid references app_user on delete set null,
  created_at timestamptz not null default clock_timestamp(),
  read_at timestamptz);
create index on notification_outbox (user_id, created_at desc);
create index on notification_outbox (request_id);
