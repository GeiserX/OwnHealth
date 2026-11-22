-- Per-profile data keys (server/crates/ownhealth-core/src/keys.rs). Each profile's files are sealed
-- under its own random 256-bit data key (DEK). The DEK is stored here wrapped with AES-256-GCM under
-- the key-encryption key HKDF-SHA256(OWNHEALTH_SECRETS_KEY, 'ownhealth kek v1'), with the profile id as
-- associated data: version (1) | nonce (12) | ciphertext (32) | tag (16). kek_fingerprint names the KEK
-- that wrapped it, so a start with another OWNHEALTH_SECRETS_KEY is refused by name.
--
-- No foreign key to profile: the app makes a profile's key on a connection of its own the first time
-- it stores one of the profile's files, while the profile may still sit in the caller's open
-- transaction. Profile delete takes this row with the profile's other rows (it has a profile_id
-- column), so a sealed file left behind no longer opens. Never exported: export.rs skips the table.
create table profile_key (
  profile_id uuid primary key,
  wrapped_dek bytea not null check (octet_length(wrapped_dek) = 61),
  kek_fingerprint bytea not null check (octet_length(kek_fingerprint) = 8),
  created_at timestamptz not null default now()
);
