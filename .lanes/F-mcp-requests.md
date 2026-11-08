# Lane F (mcp): requests for shared changes

## 1. Workspace dependencies and Cargo.lock (needs an owner's ok)

`crates/ownhealth-mcp/Cargo.toml` uses these (all already in the workspace or the lock):

- dependencies: `chrono`, `futures`, `hex`, `jsonschema`, `regex`, `rust_decimal`
- dev-dependencies: `tempfile`, `tower`, `ownhealth-ops` (the demo `base` hook, for the "demo Alex
  status under 2 KB" test)

`server/Cargo.lock` changes with them. Please keep them when you merge the lock.

## 2. Foundation: `crates/ownhealth/tests/app.rs::mcp_answers_initialize_at_slash_mcp` now fails (401)

The PAT guard runs in front of `/mcp`, as in Python 0.1.1: every request, `initialize` included,
needs a Bearer token (`test_mcp_protocol.py::test_missing_bad_and_revoked_tokens_are_401` and the
`raw_http` initialize in `test_origin_must_match_the_public_url` both send one). That test sends
initialize without a token, so it gets 401 now. Fix: create a token in the test
(`ownhealth_auth::tokens::create_token` with a `TokenIn` for a household user) and add
`authorization: Bearer <plaintext>`. A Host of `127.0.0.1` is fine; Origin is checked only when
sent.

## 3. `ownhealth_mcp::router()` serves two paths

`router(rt)` returns `/mcp` (behind the PAT guard) and `/.well-known/oauth-protected-resource` (the
RFC 9728 document the 401 points at). The app should merge it as it is; if the foundation already
mounts a well-known route, drop one of them.

## 4. Lane C: `metric_info` as a dependency (or moved to core)

`get_trend` on a wearable metric needs to know which metrics sum per day. `tools/observations.rs`
carries a copy of the list (`SUM_METRICS`: steps, active_energy, distance, sleep_asleep,
sleep_in_bed) because `ownhealth-ingest-structured` is not a dependency of the MCP crate. When lane
C exposes `metric_info` from core (or allows the dependency), the copy should go.

## 5. Known differences from Python, left as they are

- Error wording for argument validation: `jsonschema` (Rust) and pydantic word some messages
  differently. The code `bad_input` and the field are the same; only the message text can differ.
- `get_trend` `canonical_unit` when two canonical units are tied: Rust takes the first one seen.
- The tools take day boundaries (`from`, `to`, `as_of`, the daily usage day) in UTC. I did not
  confirm this against Python for an install with a non-UTC `OWNHEALTH_TIMEZONE`. The usage route
  reads "today" in `OWNHEALTH_TIMEZONE` (default UTC).

## 6. Lane A: clippy on `ownhealth-api`

`cargo clippy -p ownhealth-api --all-targets -- -D warnings` fails in
`crates/ownhealth-api/src/routes/observations.rs:563` (`map_entry`: `contains_key` then `insert`).
Not lane F code; lane F's `routes/mcp_usage.rs` and `tests/mcp_usage.rs` are clean.
