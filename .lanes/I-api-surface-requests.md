# Lane I (ownhealth-api surface): shared changes and requests

## Done outside lane I's own route files (say so in the PR)

1. **`crates/ownhealth-api/Cargo.toml`: `ownhealth-mcp` and `tokio` are now normal dependencies.**
   RUST-CONTRACTS.md section 1 says `api` uses every lane crate except `mcp`, but two Python routers
   import from `mcp/`: `api/core.py` (`GET /sources/{id}` uses `mcp.gate.genome_gate`,
   `hidden_reasons` and `READ_SCOPES`) and `api/mcp_usage.py` (`mcp.limits`). `mcp` does not depend
   on `api`, so there is no cycle. Cargo.lock changes only in the ownhealth-api package entry.
   Foundation: update the dependency line in RUST-CONTRACTS.md section 1.
2. **`crates/ownhealth-api/src/validate.rs` (new, lane I)**: FastAPI's 422 shapes for query, path
   and JSON body values (`Errors`, `param`, `bool_param`, `uuid_path`, `json_object`, `body_str`) and
   `pydantic_datetimes` (below). Any lane may use it. It builds on lane G's `routes::auth::{Fail, R,
   error_entry}`.
3. **`cargo fmt -p ownhealth-api` ran once over the whole crate**, so other lanes' files in
   `ownhealth-api` (routes and tests) may have picked up formatting-only changes. No code changed.

## Requests

1. **Foundation: wire lane C's retract into lane B in the binary.** Nothing outside tests calls
   `ownhealth_ingest_pdf::set_document_retract`. `assign_document` calls `retract_document_facts`
   every time a document changes profile, so in the running binary `POST /api/documents/{id}/assign`
   and accepting a `document_identity` review item answer 501 `not_implemented`. One line in
   `ownhealth::register_lanes()` fixes it:
   `ownhealth_ingest_pdf::set_document_retract(ownhealth_ingest_structured::retract::retract_document_facts);`
2. **Every lane with a `-> dict` route: datetimes go out with `Z`.** FastAPI serialised a route's
   return value through pydantic's JSON mode when the route declared a return type (`-> dict`,
   `-> list[dict]`), and through `jsonable_encoder` when it did not. pydantic writes a UTC datetime
   as `2026-09-14T08:00:00Z`, `jsonable_encoder` as `2026-09-14T08:00:00+00:00`, and `row_to_json`
   gives the second. Measured against the Python backend: `GET /api/review` and `GET /api/sources/{id}`
   answer `Z`, `GET /api/review/{id}` (no annotation) answers `+00:00`. Lane I's routes now match
   (`validate::pydantic_datetimes`). Other lanes' routes that return row datetimes from an annotated
   Python route should check theirs. pydantic's JSON mode also writes a `Decimal` as a string.

## Answers to other lanes

- **Lane C request 4 (the PAT message on REST writes):** no change. In 0.1.1, `get_principal`
  refused every token with "Access tokens work only on /mcp. Use the web app." before any route ran.
  The records route's "Tokens cannot write here; agents propose changes through MCP `propose`."
  message was only reachable through the test's dependency override, never over HTTP. The Rust
  `Caller` answers what 0.1.1 answered.
- **Lane C request 5:** the documents routes ended up with lane B (`routes/documents.rs` calls
  `ownhealth_ingest_structured::retract::retract_document_facts` directly for `/retract`). The
  `/assign` case needs request 1 above.

## Noticed while comparing the two backends (lane H)

- The Rust demo seed gives some sources fixed short ids (`src_3c8`, `src_3d2`, `src_a07`); the Python
  seed gives them random 8-character ids. Both match `^src_[a-z0-9_]+$`. With random ids,
  `min(o.src_id)` in the timeline picks a different row of the same draw, so a draw's cited page can
  differ between the two seeds. The API code behaves the same on both.
