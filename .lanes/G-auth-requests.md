# Lane G (auth): shared changes and requests

## Done outside lane G's own files (each one line or small; say so in the PR)

1. **`server/crates/ownhealth-api/Cargo.toml`, `[dev-dependencies]`**: one line,
   `ownhealth-auth = { workspace = true, features = ["testing"] }`. The REST tests need the software
   passkey and the ES256 signer in `ownhealth_auth::testing` (the Rust twin of 0.1.1's
   `tests/security/softkey.py`). Nothing changes for the release build: the feature is test-only.
2. **`server/crates/ownhealth-auth/Cargo.toml`** (lane G's own): feature `testing = ["dep:p256"]` with
   `p256 = { version = "0.14", default-features = false, features = ["ecdsa", "std"], optional = true }`.
   p256 0.14.0 was already in Cargo.lock through webauthn-rs-core, so the lock does not change for it.
   Also `futures`, `tempfile`, `thiserror`, `tokio` (moved from dev to normal) and
   `unicode-normalization = "0.1"` (the workspace line lane A added). Every one is already in
   Cargo.lock.
3. **`cargo fmt -p ownhealth-api`** ran once early on and may have reformatted
   `routes/genome.rs` and `routes/observations.rs` (other lanes' files, uncommitted at the time). After
   that I ran rustfmt only on lane G files. If those two files show whitespace-only diffs, that is why.

## Requests

1. **Lane H and the integrator: one `export_profile`.** There are two ports: `ownhealth_auth::export`
   (lane G, the contract's stub filled in) and `ownhealth_ops::export` (lane H). The route
   `POST /api/profiles/{p}/export` (`routes/export.rs`) calls `ownhealth_auth::export::export_profile`
   today. Both take `(conn, blobs, profile_id, out: impl Write + Seek, fmt) -> Result<Value>` (lane
   G's also asks `Send`, since the route awaits it), so switching is a one-line import change. Lane H
   checked its zip byte for byte against Python's on two databases; lane G's was checked against
   Python's `json.dumps` layouts and the route-level tests, not against a whole Python-made zip. My
   proposal: keep lane H's, delete `ownhealth_auth::export` and `pydumps.rs` if nothing else uses
   them, and point the route at `ownhealth_ops::export`. The route tests (`tests/auth_export.rs`)
   should pass unchanged.
2. **Signature change in `ownhealth_auth::export::export_profile`**: the stub's output type became
   `out: W` with `W: Write + Seek + Send` (a seekable temporary file under `<data_dir>/tmp`, as Python's
   `tempfile.mkstemp`).
3. **Lane F (MCP): token limits are not on `Principal`.** Python's token principal carried the six
   limits in `extra`; the Rust `Principal` has no `extra`, so `verify_token` does not load them. The MCP
   guard should read `rate_per_min`, `egress_facts_per_day`, `egress_bytes_per_day`,
   `genome_raw_lookups_per_day`, `document_pages_per_day` and `proposals_per_day` from `api_token` by
   `principal.token_id` (the column names are `ownhealth_auth::tokens::LIMIT_KEYS`).
4. **Lane I: `ApiError::validation(errors: Vec<Value>)`** (optional). FastAPI's 422 for a bad body or
   query is `{"detail": [ ... ]}`, a list, which `ApiError` cannot express. Lane G answers it through
   its own `routes::auth::{Fail, Body, Check}` (pydantic's entry shapes: `missing`,
   `string_too_short`, `literal_error`, `less_than_equal`, `uuid_parsing`, `int_parsing`, ...). Other
   lanes may reuse them; a shared helper in `error.rs` would let them drop `Fail`.
5. **Lane I: `/api/me`.** Lane G's tests use `/api/auth/state` and `/api/household` where 0.1.1's
   tests read `/api/me`, so they do not depend on lane I. `routes/family.rs` now calls lane I's
   `core::summaries` and `core::local_today` for `/household`.
6. **Optional, foundation: `[profile.dev.package.scrypt] opt-level = 3`** in `server/Cargo.toml`.
   scrypt N=16384 in a debug build takes about a second per hash, so the password tests spend most of
   their time there (`auth_passkeys` 12 s, `auth_python_rows` 5 s). Not needed for correctness.

## Differences from 0.1.1 that remain (small)

- A 422 entry for a field of the wrong JSON type (a number where a string goes) is one
  `value_error` entry with serde's message, not pydantic's exact type and message. Missing fields,
  lengths, literals, ranges, dates, uuids, ints and booleans give pydantic's types and `loc`.
- The `uuid_parsing` message carries the uuid crate's reason after "Input should be a valid UUID, ",
  not pydantic's.
- The export response has `content-length` but not Starlette `FileResponse`'s `etag` and
  `last-modified` headers.
