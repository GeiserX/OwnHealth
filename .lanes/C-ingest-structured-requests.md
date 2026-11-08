# Lane C (ingest-structured) requests to the foundation

1. **Workspace dependency `unicode-normalization = "0.1"`** (another lane already added the line in `server/Cargo.toml`;
   resolves to 0.1.26, already in Cargo.lock through sqlx). `mapping.name_tokens` is Python's
   `unicodedata.normalize("NFKD", s).encode("ascii", "ignore")`; patient matching and the manual
   fallback alias group depend on it byte for byte.
2. **`retract_document_facts` for lane B.** Python's `ingest/pdf.assign_document` calls
   `ingest.retract.retract_document_facts` (and through it `wearables.recompute_rollups`). Both are
   ported in `ownhealth_ingest_structured::{retract, wearables}`. The dependency graph in
   RUST-CONTRACTS.md section 1 does not let `ingest-pdf` depend on `ingest-structured`. Proposal: allow
   `ownhealth-ingest-pdf -> ownhealth-ingest-structured` (C does not depend on B, so no cycle). The
   documents retract route (`api/documents.py`) is in `ownhealth-api`, which may already call it.
3. **Python's `random.Random`** (MT19937 with `init_by_array` seeding, `random`, `randint`, `uniform`)
   is ported in `ownhealth_ingest_structured::pyrandom` for the synthetic Apple Health export of the
   demo hook. Lane H's base seed needs the same generator (`random.Random(20261107).uniform`). If H
   should not depend on this crate, move `pyrandom.rs` into `ownhealth-core` unchanged.
4. **PAT refusal message on REST writes (lane I / G).** Python's records routes answered a token
   write with 403 "Tokens cannot write here; agents propose changes through MCP `propose`." The Rust
   `Caller` extractor refuses every token earlier with "Access tokens work only on /mcp. Use the web
   app.", so the token case of `test_record_validation_and_permissions` (asserts "propose" in the
   message) is not ported in `ownhealth-api/tests/records.rs`. If the message should match, change it
   in `extract.rs`; the status (403) already matches.
5. **Lane B / I tests that call lane C.** `test_document_retract.py` cases that go through
   `assign_document` (moving a FHIR or Apple document to another profile, the genome-facts refusal)
   and the `/documents/{id}/retract` endpoint are not ported here; they belong with `assign_document`
   (lane B) and the documents routes (lane I), and need request 2.
