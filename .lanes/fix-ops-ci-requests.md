# fix-ops-ci: requests for files outside the lane

Status: all done in the 0.2.0 gate (feat/rust-backend). Request 2 of the ops-ci list changed with the Python tree gone: the MCP client contract now runs the pinned client through `uv run --script` and fails in CI when uv is missing.

The ops-ci fix lane owns `ownhealth-ops`, Dockerfile, compose, `deploy/`, `.github/`, `scripts/`,
`contract-tests/` and `docs/`. These parts of its findings sit in other lanes' files.

## 1. lane I (`ownhealth-api`): the export route's file IO off the async worker

Finding: profile export does zip and blob IO on a tokio worker inside an open transaction.
`ownhealth_ops::export::export_profile` now runs its blob copies, `close_lines` and `finish` through
`ownhealth_ops::export::blocking` (`block_in_place` on the multi-thread runtime, inline elsewhere);
regression test `ownhealth-ops/tests/export.rs::a_large_export_does_not_stall_the_runtime` (fails
without the fix: a 2 ms sleep took 12.6 s during a 12.8 s export).

Left in `server/crates/ownhealth-api/src/routes/export.rs` (`export`): wrap the blocking calls in the
same helper, e.g.

```rust
use ownhealth_ops::export::{blocking, export_profile};
let (path, out) = blocking(|| -> std::io::Result<_> {
    std::fs::create_dir_all(&tmp_dir)?;
    let path = tmp_dir.join(format!("export-{}.zip", Uuid::new_v4().simple()));
    let out = OpenOptions::new().create_new(true).read(true).write(true).open(&path)?;
    Ok((path, out))
}).map_err(internal)?;
// TmpFile(path.clone()) right after, as now
...
let size = blocking(|| std::fs::metadata(&path)).map_err(internal)?.len();
...
let file = blocking(|| File::open(&path)).map_err(internal)?;
```

## 2. lane F (`ownhealth-mcp`): the Python client contract must not skip in CI

`.github/workflows/ci.yml` `server` job now installs uv 0.11.29 and runs `uv sync --frozen` in
`backend/` before `cargo test`, so `backend/.venv` exists there.

Left in `server/crates/ownhealth-mcp/tests/python_client.rs` (lines 16-19): when the venv is
missing and `std::env::var_os("CI").is_some()`, fail instead of returning `Ok(())`:

```rust
if !python.is_file() {
    if std::env::var_os("CI").is_some() {
        anyhow::bail!("backend/.venv absent in CI: run `uv sync --frozen` in backend/ first");
    }
    eprintln!("backend/.venv absent: skipping the Python client contract");
    return Ok(());
}
```

(Use whatever error type `common::R` takes.) Also update the module doc: "Skipped when the venv is
absent, except in CI."

## 3. foundation (`ownhealth` bin): a per-request access log

`server/crates/ownhealth/src/app.rs` `build_app`: add `tower_http::trace::TraceLayer` to the merged
router (tower-http `trace` is already a workspace feature; `ownhealth` already depends on
tower-http and tracing-subscriber with EnvFilter in `main.rs`). One INFO line per response with
method, matched path (`axum::extract::MatchedPath`, falling back to the URI path without the query),
status and latency; never the query string, headers or bodies. Example:

```rust
use axum::extract::MatchedPath;
use tower_http::trace::TraceLayer;
let trace = TraceLayer::new_for_http()
    .make_span_with(|req: &axum::http::Request<_>| {
        let path = req.extensions().get::<MatchedPath>().map_or(req.uri().path(), MatchedPath::as_str);
        tracing::info_span!("http", method = %req.method(), path = %path)
    })
    .on_request(())
    .on_response(|res: &axum::http::Response<_>, latency: std::time::Duration, _: &tracing::Span| {
        tracing::info!(status = res.status().as_u16(), latency_ms = latency.as_millis() as u64, "request");
    });
```

`RUST_LOG=info,tower_http=off` (or a target filter on the span) switches it off. A test: drive
`/healthz` through `build_app` with a captured subscriber and assert one line with `GET`,
`/healthz` and `200`, and that `?x=secret` never appears.

## 4. foundation (`ownhealth` bin, `ownhealth-core` blobs): sweep stale upload spools

`server/crates/ownhealth-api/src/routes/imports.rs` spools to `<blobs>/tmp/upload-<hex>`;
`BlobStore::put_reader`/`put_async` write `<blobs>/tmp/<32 hex>`. A killed process leaves them in
the persistent `/data` volume. Add e.g. `BlobStore::sweep_tmp(older_than: Duration) -> io::Result<usize>`
in `ownhealth-core/src/blobs.rs` that removes regular files in `root/tmp` named `upload-*` or 32 hex
chars whose mtime is older than the bound, and call it with 1 hour from `app::start` and from the
`worker` path at start-up (log the count at INFO when non-zero). Age check so another process's
live upload is untouched. Test: one old file (set mtime back with `File::set_modified`) is removed,
one fresh file and one unrelated name stay.

## 5. AGENTS.md and CONTRIBUTING.md (repo root): the dev loop is the Rust server

`docs/`, `deploy/` and `contract-tests/` already point at the right server (the remaining
`uv run` lines there are the reference backend, labelled as such). The root docs still teach the
Python backend as the server to run. Proposed `AGENTS.md` "Dev commands" head:

````markdown
Server (Rust, the shipped binary), from `server/`:

```sh
../scripts/test-db.sh                 # shared test Postgres 17 on 127.0.0.1:55432 (idempotent)
bash ../scripts/fetch-pdfium.sh       # pinned PDFium for this machine, once
mkdir -p static                       # the UI build is embedded only in release builds
cargo test -p <crate>                 # one crate; each test creates and drops its own database
cargo fmt --all --check && cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p ownhealth -- migrate     # apply migrations to OWNHEALTH_DATABASE_URL
cargo run -p ownhealth -- demo-seed   # seed the synthetic Rivera family (--force to add another)
OWNHEALTH_DEV_AUTH=1 OWNHEALTH_DEMO=1 cargo run -p ownhealth -- serve   # http://127.0.0.1:8080, MCP at /mcp
```

Contract suite, from the repo root: `bash contract-tests/run.sh rust` (and `python` for the reference).

Reference backend (Python 0.1.1, the behaviour spec; not shipped), from `backend/`:
`uv sync`, `uv run pytest -q`, `uv run ownhealth serve`.
````

`CONTRIBUTING.md` "Development setup" block (line 20) becomes:

```sh
./scripts/test-db.sh                  # test Postgres 17 on 127.0.0.1:55432
bash scripts/fetch-pdfium.sh && cd server && cargo test -p <crate>
cd .. && bash contract-tests/run.sh rust
cd ui && pnpm install && pnpm run check && pnpm build
```

and the backup sentence names `server/crates/ownhealth-ops/tests` (they need pg_dump/psql 17 and
restic on PATH or `OWNHEALTH_PG_BINDIR`). The "How to add a lab template" test step (line 69,
`cd backend && uv run pytest -q tests/pdf`) should name the Rust PDF tests
(`cargo test -p ownhealth-ingest-pdf`), keeping the Python command only as the reference check.
