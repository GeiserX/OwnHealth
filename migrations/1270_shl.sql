-- 1270, v1.0 Connected, sharing (1270-1279): SMART Health Links that the instance serves itself.
--
-- A link is a manifest URL https://<share host>/api/shl/<id43> plus a 32-byte key. Both live only in
-- the link the owner or a guardian hands out (security-privacy.md section 11): the server keeps the
-- sha256 of the id, never the id, and never the key. The one file is a compact JWE (dir + A256GCM)
-- of the profile's International Patient Summary, encrypted under that key at creation.
--
-- Access control and ciphertext, not clinical rows: no lifecycle trigger, and a person's export leaves
-- the tables out (ownhealth-ops export.rs SKIP_RAW). Deleting the profile removes its links, files and
-- access log. Revoking a link deletes its file.
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
  check ((flag = 'P') = (passcode_hash is not null and attempts_left is not null)));
create index on shl (profile_id, created_at);

create table shl_file (
  id uuid primary key default gen_random_uuid(),
  shl_id uuid not null references shl on delete cascade,
  content_type text not null,
  jwe text not null,
  created_at timestamptz not null default now());
create index on shl_file (shl_id);

-- One row per answer to a recipient: a manifest, a file, or a wrong passcode. A manifest row carries
-- the sha256 of its file location ticket, which opens the file for at most one hour.
create table shl_access (
  id bigint generated always as identity primary key,
  shl_id uuid not null references shl on delete cascade,
  at timestamptz not null default now(),
  kind text not null check (kind in ('manifest', 'file', 'passcode_wrong')),
  recipient text not null check (char_length(recipient) between 1 and 200),
  ip inet,
  user_agent text,
  ticket_sha256 bytea unique,
  ticket_expires_at timestamptz,
  check ((ticket_sha256 is null) = (ticket_expires_at is null)));
create index on shl_access (shl_id, at);
