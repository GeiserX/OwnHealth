# Requests from the fix-core-mcp-auth lane

Status: all done in the 0.2.0 gate (feat/rust-backend).

## 1. To the owner of ownhealth-ingest-pdf: call the retract directly, drop the RETRACT seam

Finding (low): `server/crates/ownhealth-ingest-pdf/src/import.rs:69-90` keeps a process-global
`static RwLock<Option<DocumentRetract>>`. When nothing installed it, `retract_document_facts`
returns `Error::NotImplemented`. The comment says lane B cannot depend on lane C's crate, but
`ownhealth-ingest-structured` depends only on core and normalize, and ingest-pdf already lists it as a
dev-dependency, so a normal dependency makes no cycle.

Asked for:
- Make `ownhealth-ingest-structured` a normal dependency of `ownhealth-ingest-pdf` and call
  `ownhealth_ingest_structured::retract::retract_document_facts` directly.
- Delete `RETRACT`, `set_document_retract`, and the call in `ingest-pdf/tests/support/env.rs:29`.
- Tell this lane when it has landed. We then delete the call in `ownhealth::register_lanes` and in
  `ownhealth-api/tests/documents.rs:34`, `Error::NotImplemented` itself, and its match arms in
  core/src/jobs.rs, api/src/error.rs, mcp/src/resources.rs, mcp/src/tools/source.rs,
  mcp/src/tools/propose.rs and mcp/src/tools/mod.rs. The MCP tests' `StateMode::Stub` fake, which
  produces the variant today, goes at the same time. Deleting the variant first would break your crate.

## 2. To the owner of ownhealth-rules: the SOURCE global in rules/src/genome.rs

Same finding: `server/crates/ownhealth-rules/src/genome.rs:37-50` is a process-global test seam in
production code. Swap it for a trait object passed in, as the MCP crate's `Services` does, or put it
behind `#[cfg(feature = "testing")]`.

## 3. To the owner of ownhealth-ingest-structured: port test_a_structured_import_with_no_stored_blob_fails_for_good

Finding (medium, missing tests): `backend/tests/test_jobs.py:262-274` checks that `import.fhir_bundle`
fails for good (`PermanentJobError`) for 3 payloads: no `blob_sha256`; `"ab" * 32`, which is not in the
store; and `"not-hex"`. Each one also carries `household_id` "x" and `profile_id` "y". Rust has no
counterpart for `ingest-structured/src/jobs.rs` `document_for`. Please port it to
`ownhealth-ingest-structured/tests` and assert `JobError` is permanent for each payload. Watch it fail
once by mutating `document_for`, for example by making the missing-blob branch a retryable error.

## 4. To every owner: consider banning serde_json::Map::remove workspace-wide

With serde_json's `preserve_order`, `Map::remove` is `swap_remove`. It moves the last key into the
hole, so a JSON answer's keys stop matching 0.1.1's order. This lane put
`disallowed-methods = ["serde_json::Map::remove"]` in crate-level `clippy.toml` files for
ownhealth-mcp, ownhealth-api and ownhealth-core. We did not add it to the workspace file, because
`ownhealth-genome/tests/schema.rs:72,84,218,221` and `ownhealth-genome/tests/ingest.rs:378` call
`Map::remove` and would then fail `clippy -D warnings`. Ordering does not matter there. If the genome
lane switches those calls to `shift_remove`, the ban can move to `server/clippy.toml` and cover every
crate, including rules and ops, which also build JSON answers.
