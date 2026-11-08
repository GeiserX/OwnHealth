# Lane D (genome): requests for shared files

## 1. Dependencies (Cargo.toml / Cargo.lock)

`crates/ownhealth-genome/Cargo.toml` uses these workspace dependencies: `base64`, `hex`, `regex`, `sha2`, and `tempfile` as a dev-dependency. They are already in the workspace table, so Cargo.lock only gains edges, no new packages. Please keep those edges when you merge the lock.

## 2. Contract changes from RUST-CONTRACTS.md

- `lookup_variant(conn, blobs, profile_id, &Gate, &VariantQuery) -> Result<Result<Value, GenomeQueryError>>`. The outer error is a core failure. The inner error is a query the lookup cannot answer, with `code` one of rsid_unknown, hgvs_unresolved, region_too_large, bad_input or no_genome, and `as_value()` giving `{code, message, fix}`. Python raised `GenomeQueryError`; callers turn it into a 400 or 404 (REST) or a tool error (MCP). Lane F's `ownhealth-mcp/src/services.rs` already matches on it.
- `VariantQuery { rsid, hgvs, region, cursor }` and `FindingsQuery { category, gene, min_evidence (None = E2), as_of }` are the query structs.
- Additions: `list_findings_typed -> Findings` (the typed struct behind `list_findings`), `findings::pgx_for_drug_with(conn, p, cpic_drug_id, as_of, genes, drug_name)`, `findings::visible_rows`, `withheld_reason`, `gate_for`, `load_ref_sites`, `fact_id`, `validate_facts`, and `synthetic::*` (fixture builders, also used by the API tests).
- `Gate { surface, genome, raw, sensitive, minor }`: `sensitive` is Python's `include_sensitive`, and `minor` is the minor rule. `Gate::new(include_sensitive, minor)` sets surface "agents" with genome and raw true. `gate_for(perm, "ui"|"api"|"agents"|"mcp", on, age_of_majority)` is the Python `gate_for`; an unknown surface is `Error::Invalid`.

## 3. Reliance on lane G

`routes/genome.rs` POST /genome/consent calls `ownhealth_auth::sessions::require_step_up(conn, principal, 300)`. Dev auth passes it, a session needs `step_up_at` within 300 s. The API tests use real session cookies (`create_session_with(.., Some(step_up))`), so if G changes that function's signature or meaning, `tests/genome.rs` will show it.

`routes/genome.rs` uses lane G's `routes::auth::{Body, Check, Fail, R, detail, ok}` for pydantic-shaped 422s and `{"detail": {...}}` answers.

## 4. New fixtures

`fixtures/genome/htslib/`: AR-01.vcf.gz(.tbi) and AR-01.g.vcf.gz(.tbi), written by pysam/htslib from the same synthetic rows. `tests/lookup.rs::htslib_made_files_answer_the_same` reads them, to show that noodles reads htslib's index the way Python did. They hold synthetic data only. Its README says how to make them again.

## 5. Shared target dir trap (for the orchestrator)

On 2026-11-08 the genome tests failed with `relation "household" does not exist`. The ownhealth-core rlib in the shared target dir had been built from another worktree (`/Volumes/Data/scratch/ownhealth/h-ops-check`, now deleted). rust-embed in a debug build reads migrations at runtime from the absolute path fixed at compile time, so `migrate::embedded()` returned no migrations, and every `TestDb::new()` gave an empty database. `strings target/debug/deps/<test>-* | grep db/migrations` shows the path. Touching `crates/ownhealth-core/src/migrate.rs` (mtime only) forced a rebuild and fixed it. Any lane building from another checkout with the same `CARGO_TARGET_DIR` can cause this again.
