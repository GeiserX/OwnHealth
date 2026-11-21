-- 1270, v1.0 Connected, sharing (1270-1279): SMART Health Links that the instance serves itself.
--
-- A link is a manifest URL https://<share host>/api/shl/<id43> plus a 32-byte key. Both live only in
-- the link the owner or a guardian hands out (security-privacy.md section 11): the server keeps the
-- sha256 of the id, never the id, and never the key. The one file is a compact JWE (dir + A256GCM)
-- of the profile's International Patient Summary, encrypted under that key at creation.
--
-- Access control and ciphertext, not clinical rows: no lifecycle trigger, and a person's export leaves
-- the tables out (ownhealth-ops export.rs SKIP_RAW). Deleting the profile removes its links, files and
-- access log. Revoking a link deletes its file, and so does running out of passcode attempts; the
-- scheduler deletes the files of expired links.
create table shl (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile on delete cascade,
  created_by uuid not null references app_user on delete cascade,
  manifest_sha256 bytea not null unique,
  label text check (char_length(label) between 1 and 80),
  -- the SHL payload flag: P when a passcode guards the manifest
  flag text not null check (flag in ('', 'P')),
  passcode_hash text,
  -- wrong passcodes still allowed over the life of the link; at 0 the link answers 404
  attempts_left int check (attempts_left >= 0),
  categories text[] not null,
  lang text not null check (lang in ('es', 'en')),
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  revoked_at timestamptz,
  -- why it was revoked: by the person, by a lock on a category it carries, or because its creator
  -- lost a category it carries (shl_end_unshareable below)
  revoked_reason text check (revoked_reason in ('person', 'locked', 'access_changed')),
  check ((flag = 'P') = (passcode_hash is not null and attempts_left is not null)),
  check ((revoked_at is null) = (revoked_reason is null)));
create index on shl (profile_id, created_at);

create table shl_file (
  id uuid primary key default gen_random_uuid(),
  shl_id uuid not null references shl on delete cascade,
  content_type text not null,
  jwe text not null,
  created_at timestamptz not null default now());
create index on shl_file (shl_id);

-- One row per answer to a recipient: a manifest, a file, a wrong passcode, or a request with no
-- passcode on a link that needs one (it costs no attempt, but the owner sees it and it counts toward
-- the hourly limit). A manifest row carries the sha256 of its file location ticket, which opens the
-- file for at most one hour. ip is the client address (behind a trusted proxy, the one it forwarded).
create table shl_access (
  id bigint generated always as identity primary key,
  shl_id uuid not null references shl on delete cascade,
  at timestamptz not null default now(),
  kind text not null check (kind in ('manifest', 'file', 'passcode_wrong', 'passcode_missing')),
  recipient text not null check (char_length(recipient) between 1 and 200),
  ip inet,
  user_agent text,
  ticket_sha256 bytea unique,
  ticket_expires_at timestamptz,
  check ((ticket_sha256 is null) = (ticket_expires_at is null)));
create index on shl_access (shl_id, at);

-- A lock ends a link; it does not pause it. When a link's creator may no longer share every category
-- it carries, the link is revoked at that moment and its file deleted, so lifting the lock or widening
-- the grant later brings back neither the link nor a file location it handed out. The rule is the one
-- routes/shl.rs live() checks on every request: the creator holds an owner or guardian grant on the
-- profile that is not break-glass and not expired, the creator's login is open, and every category of
-- the link is in the grant and not in the profile's locks (the owner's own locks too). Links already
-- revoked, expired or out of attempts are left as they are. `target` null: every profile (the
-- scheduler, for grants that run out by themselves). Returns the links ended.
create function shl_end_unshareable(target uuid, why text) returns int language plpgsql as $$
declare
  n int;
begin
  with ended as (
    update shl s set revoked_at = now(), revoked_reason = why
    where (target is null or s.profile_id = target)
      and s.revoked_at is null and s.expires_at > now()
      and (s.attempts_left is null or s.attempts_left > 0)
      and not exists (
        select 1 from profile_grant g
        join app_user u on u.id = g.user_id
        join profile p on p.id = g.profile_id
        where g.user_id = s.created_by and g.profile_id = s.profile_id
          and g.role in ('owner', 'guardian') and not g.break_glass
          and (g.expires_at is null or g.expires_at > now()) and u.disabled_at is null
          and s.categories <@ g.categories and not (s.categories && p.locks))
    returning s.id),
  files as (delete from shl_file f using ended e where f.shl_id = e.id)
  select count(*) into n from ended;
  return n;
end $$;

-- Every write that can take a category from a creator runs the rule for that profile: a lock change,
-- a grant changed or removed, a login closed. A profile being deleted takes its links with it.
create function shl_on_access_change() returns trigger language plpgsql as $$
begin
  if tg_table_name = 'profile' then
    perform shl_end_unshareable(new.id, 'locked');
  elsif tg_table_name = 'profile_grant' and tg_op = 'DELETE' then
    if exists (select 1 from profile where id = old.profile_id) then
      perform shl_end_unshareable(old.profile_id, 'access_changed');
    end if;
  elsif tg_table_name = 'profile_grant' then
    perform shl_end_unshareable(new.profile_id, 'access_changed');
  else
    perform shl_end_unshareable(s.profile_id, 'access_changed')
      from (select distinct profile_id from shl where created_by = new.id and revoked_at is null) s;
  end if;
  return null;
end $$;

create trigger shl_lock after update of locks on profile
  for each row when (new.locks is distinct from old.locks) execute function shl_on_access_change();
create trigger shl_grant after update or delete on profile_grant
  for each row execute function shl_on_access_change();
create trigger shl_login after update of disabled_at on app_user
  for each row when (new.disabled_at is not null and old.disabled_at is null)
  execute function shl_on_access_change();
