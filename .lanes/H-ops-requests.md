# Lane H (ownhealth-ops): shared changes and requests

## Done outside crates/ownhealth-ops (each one small, say so in the PR)

1. **`crates/ownhealth/src/main.rs`: logs go to stderr** (`.with_writer(std::io::stderr)`). tracing's
   default writer is stdout, so `ownhealth verify-restore` and `ownhealth restore` printed log lines
   before their JSON report. That broke `deploy/restic/restore-test.sh`, cron logs and the ported
   restic test: `restore_test_fails_when_the_latest_snapshot_does_not_match_its_manifest` failed with
   "expected value at line 1 column 1" before the fix. Python's logging went to stderr.
2. **`crates/ownhealth/tests/ops_cli.rs` (new file)**: the CLI and restic recipe tests need
   `CARGO_BIN_EXE_ownhealth`, which only the binary's own package gets. Lane H owns that file.
3. **`Dockerfile`, `.github/workflows/ci.yml`, `.github/workflows/release.yml`**: rewritten for the
   Rust image, as the H-ops brief asks. Notes for integration are below.
4. **`crates/ownhealth-ops/Cargo.toml`**: added `chrono futures hex regex rust_decimal sha2 tempfile
   uuid zip` as normal dependencies. Every one is already in `[workspace.dependencies]` and in
   Cargo.lock, so Cargo.lock changes only in the ownhealth-ops package entry.

## Requests

1. **Lane G and the foundation: one `export_profile`.** The H-ops brief gives `export/` to lane H, but
   RUST-CONTRACTS.md section 1 lists it under lane G (a stub in `ownhealth_auth::export`). The port is in
   `ownhealth_ops::export::export_profile(conn, blobs, profile_id, out: impl Write + Seek, fmt) ->
   Result<Value>`. Every entry of its zip is byte-identical to Python's export of the same database
   (checked on a Rust-seeded and a Python-seeded demo database, Apple Health, FHIR and genome data
   included; the manifest differs only in `generated_at` and `ownhealth_version`). Proposal: lane G's
   `POST /profiles/{p}/export` route (`ownhealth-api/src/routes/export.rs`) calls
   `ownhealth_ops::export::export_profile` (`api` may depend on every lane crate), and the
   `ownhealth_auth::export` stub goes away. `out` must be seekable: write into a temp file under
   `<data_dir>/tmp` as Python did. `ownhealth_auth` cannot depend on `ownhealth_ops`, so the stub cannot
   simply forward.
2. **Foundation: drop the `/metrics` route, or say what it should serve.** 0.1.1 had no `/metrics`:
   Python answered the SPA `index.html` there, as for any unknown non-API path. The Rust app routes it
   to `ownhealth_ops::metrics`, which answers 501. architecture.md 15 plans metrics on their own port
   or behind a token, which is new behaviour and outside "the same behaviour as 0.1.1". Lane H left
   `metrics` as an explicit `NotImplemented` and did not invent an endpoint.
3. **Foundation: one Python `random.Random`.** Lane C has `ownhealth_ingest_structured::pyrandom` and
   lane H has `ownhealth_ops`'s private `pyrandom` (MT19937 + `init_by_array`, `random`, `uniform`, and
   `round1` for Python's `round(v, 1)`). Both are checked against Python. If you move one into
   `ownhealth-core`, H switches to it; until then H's copy stays private.
4. **Lane B: confirm the PDFium path.** The image installs the library at
   `/usr/local/lib/libpdfium.so` (with `ldconfig`) and sets `OWNHEALTH_PDFIUM_LIB` to that file path.
   If the loader expects a directory in that variable, tell lane H or change the Dockerfile's `ENV`.

## For integration (not changed by lane H)

- `ownhealth-ops` backup, restore and verify-restore call `ownhealth_rules::build_state` and
  `rules_version`. Until lane E lands them, 10 of the 11 tests in `crates/ownhealth-ops/tests/backup.rs`
  and all 4 in `crates/ownhealth/tests/ops_cli.rs` fail with "lane E: rules::build_state is not
  implemented yet". With a stand-in `build_state` (a scratch copy only) all 15 pass, and three
  deliberate breaks (commit instead of rollback, no blob pre-check, no corrupt check) turn the expected
  tests red.
- The ops tests need pg_dump/pg_restore 17: on PATH, or `OWNHEALTH_PG_BINDIR=$PWD/../backend/tests/ops/bin`
  (the wrapper that runs them in `ownhealth-testpg`). The restic tests skip without `restic` unless `CI`
  is set. CI installs both.
- The release builds each platform on its own runner (`ubuntu-latest`, `ubuntu-24.04-arm`) and merges
  by digest; compiling the workspace under QEMU for arm64 would take hours. The image, the multi-arch
  build and the CI docker smoke (`/readyz`, the UI, `healthcheck`, a backup and its verify-restore inside
  the container, `ldd` of libpdfium) were not run on this box: no image builds here.
- `contract` CI job: `contract-tests/run.sh python|rust`, a matrix of two. It starts `ownhealth-testpg`
  through `scripts/test-db.sh`, as on a box.
- Rust toolchain pinned to 1.98.1 in CI (`RUST_TOOLCHAIN`) and in the Dockerfile (`rust:1.98.1-slim-trixie`),
  the version the lanes run clippy with here. Bump both together.
