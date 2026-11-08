-- 0600 lane G: what logins need beyond 0001 (security-privacy.md section 5).

-- One-time, short-lived secrets: WebAuthn challenges, OIDC state, invite links.
-- `challenge` holds the raw WebAuthn challenge, the OIDC state, or sha256 of an invite code.
create table auth_challenge (
  id uuid primary key default gen_random_uuid(),
  purpose text not null check (purpose in ('setup', 'register', 'login', 'step_up', 'enroll', 'invite', 'oidc')),
  challenge bytea not null unique,
  user_id uuid references app_user on delete cascade,
  session_id uuid references user_session on delete cascade,
  data jsonb not null default '{}',
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz);
create index on auth_challenge (expires_at);

-- Password fallback, only for installs where WebAuthn cannot run (plain HTTP on a LAN address).
-- scrypt, encoded as 'scrypt$n$r$p$salt_b64$hash_b64'.
alter table app_user add column password_hash text;

-- How the session started, so step-up knows which ceremony to ask for.
alter table user_session add column method text not null default 'passkey'
  check (method in ('passkey', 'recovery', 'oidc', 'password'));
