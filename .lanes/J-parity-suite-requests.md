# J-parity-suite: requests to other lanes

The contract suite is `contract-tests/` (17 test files, 77 tests, 103 goldens recorded from Python 0.1.1 on a
fresh demo database). `bash contract-tests/run.sh python` passes, and `bash contract-tests/run.sh rust` runs
every binary (`--no-fail-fast` inside run.sh; its arguments did not change). A golden is a skeleton of keys and
value kinds, with enum values kept literally; record again with `OWNHEALTH_CONTRACT_RECORD=1 bash
contract-tests/run.sh python`, never from Rust.

## Shared files

- `server/Cargo.lock`: `ownhealth-contract-tests` gained edges to anyhow, base64, regex, reqwest, serde_json,
  sha2, tokio and zip, all `workspace = true`. No new crate entered the lock.

## Rust failures on 2026-11-08 (13 of 77)

A shape test stops at its first failing golden, so fixing one can uncover the next.

1. **Timestamps: Z vs +00:00 (lanes I, G, C, A).** Python writes these fields with `Z`, and Rust writes them
   with an offset:
   - `token.created_at`, `token.expires_at` (tokens_create)
   - `household.grants[].granted_at`
   - `records/*[].recorded_at`, `detail.effective_at` (FHIR condition)
   - `manual_observation.effective_at`
   - `document_uploaded.collected_at`, `documents[].collected_at`
   - `wearables/summary devices[].last_sample`

   Some other Python fields are written with an offset (for example `review_item.created_at`), so the format
   has to match field by field. One global switch would be wrong.
   Tests: tokens::create_list_and_revoke, shapes::me_profiles_household_and_auth_state,
   shapes::records_wearables_and_timeline, shapes::documents_pages_and_sources,
   records::add_correct_retract_and_history_of_each_record_kind,
   records::manual_observations_resolve_and_show_everywhere,
   imports::a_labnorte_pdf_auto_accepts_clean_rows_and_queues_the_rest,
   imports::a_fhir_ips_bundle_projects_records_and_queues_the_rest,
   imports::an_apple_health_export_becomes_daily_wearable_values.
2. **MCP initialize capabilities (lane F).** Python sends `capabilities.tools.listChanged`,
   `prompts.listChanged`, `resources.listChanged` and `resources.subscribe`, and Rust leaves them out.
   Test: mcp_protocol::both_protocol_versions_on_one_stateless_endpoint.
3. **MCP resources/list (lane F).** Rust adds `cacheScope: "private"` and `ttlMs: 60000`, which Python does not
   send. Test: mcp_protocol::resources_templates_and_prompts.
4. **FastAPI 422 body (lane I).** For `wearables/summary?days=3`, Python includes `"ctx": {"ge": 7}` in the
   error and Rust does not. Test: rest_errors::error_bodies_of_the_reads.
5. **Document assign (lane C).** `POST /api/documents/{id}/assign` answers 501 `not_implemented` ("lane C:
   retract_document_facts (set_document_retract) is not implemented yet").
   Test: review::assigning_a_document_moves_it_and_reimports.

## Found in 0.1.1 (pinned as the spec, or tolerated)

- **MCP `check_drug` with an unknown drug returns `internal_error`, while REST returns 404 `drug_unknown`.**
  This looks like a 0.1.1 bug. The test requires `internal_error` only when the backend is Python. On Rust it
  only checks that the result is an error, so Rust may answer `drug_unknown`.
- **Python commits a request's transaction after its response has gone out.** `get_conn` is a yield
  dependency, so a token, relative or revoke used at once can still be invisible, for example a 401 on a token
  that was just created. The suite copes in two ways. `Client::settle` polls until the write shows up, and on
  the Python backend each successful write waits 40 ms. Rust should commit before it responds, and the suite
  adds no wait for Rust.
- **`POST /api/aliases` reports `rederived` as the count over every profile in the household.** That count
  depends on what other tests left behind, so the suite checks only that it is an integer.

## Not covered (impossible or out of reach at run.sh's base URL)

- Passkey ceremonies. run.sh serves on `127.0.0.1`, and 0.1.1 offers passkeys only on `localhost` or HTTPS, so
  the suite only checks that passkeys are unavailable there.
- First-run setup success. The demo seed always creates an owner, so the suite checks `already_set_up` 409
  only.
- OIDC (off in the demo), and session idle and absolute expiry (12 h and longer).
- Serving the built UI.
