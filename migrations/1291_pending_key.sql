-- Pending-item data keys (server/crates/ownhealth-core/src/keys.rs). Bytes that arrive with no profile
-- yet (a held mail, a document with no profile) are sealed under a random 256-bit key of their own item,
-- wrapped as profile_key wraps a profile's, with the item id as associated data under another label:
-- version (1) | nonce (12) | ciphertext (32) | tag (16). When the item gets its profile (a mail released,
-- a document assigned), its files are sealed again under the profile's key, and this key is shredded in
-- the same transaction: the row stays with wrapped_dek and kek_fingerprint null, so a key write that races
-- it waits on the row and then refuses. A deleted held mail is shredded the same way.
--
-- No foreign key: a held mail's key is made before its quarantine_item row, on a connection of its own.
-- Keys of items that no longer wait for a profile are shredded after a day (keys.rs
-- shred_pending_orphans). Never exported: the table has no profile_id column.
create table pending_key (
  item_id uuid primary key,
  wrapped_dek bytea check (octet_length(wrapped_dek) = 61),
  kek_fingerprint bytea check (octet_length(kek_fingerprint) = 8),
  created_at timestamptz not null default now(),
  check ((wrapped_dek is null) = (kek_fingerprint is null))
);
