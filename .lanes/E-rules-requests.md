# Lane E (rules/state): requests for shared files

## 1. Cargo.lock: new edges from ownhealth-rules (foundation)

`crates/ownhealth-rules/Cargo.toml` now uses `anyhow`, `hex`, `regex`, `sha2` and `thiserror`, all
`.workspace = true`. No new crate enters the workspace, but `Cargo.lock` gains these edges under
`ownhealth-rules`. Please keep them when you settle the lock.

## 2. Full NFKD for accent folding (optional, foundation)

Python's drug and alias matching folds accents with `unicodedata.normalize("NFKD", ...)`. The workspace
has no Unicode normalization crate, so `ownhealth-rules` folds with a Latin table (á é í ó ú ü ñ ç and
the other Latin-1/Latin Extended-A letters). That covers every name in the shipped pack and the CPIC
snapshot. If a non-Latin or compatibility-form name should ever match, add `unicode-normalization`
(or `icu_normalizer`) to `[workspace.dependencies]` and I will switch `engine::casefold` and
`drugs::norm_value` to it.

## 3. Lane D: keep the keys of list_findings (lane D)

`ownhealth_rules::genome::list_findings` reads `ownhealth_genome::list_findings(conn, pid,
&Gate::new(false, minor), &FindingsQuery { min_evidence: Some("E2"), as_of: Some(as_of), .. })` as JSON
and uses `findings`, `withheld_sensitive`, `withheld_minor` and `assay`. It also reads `pgx_for_drug(conn,
pid, cpic_drug_id, Some(as_of))`. Please keep those names and shapes; state_hash parity depends on them.

## 4. The base demo seed (lane H)

The Python base demo seed draws its wearable values from Python's RNG. Lane E's tests do not wait for a
Rust port: they load a pg_dump of the Python seed (`crates/ownhealth-rules/tests/fixtures/seed.sql`,
synthetic Rivera family, written by `tests/fixtures/make_fixtures.py`) and compare against the Python
outputs in `expected.json`. If lane H's Rust seed must give the same state_hash as Python, it has to
reproduce those rows byte for byte; the parity test in `ownhealth-rules/tests/parity.rs` is the check.

## 5. ownhealth-api is red for reasons outside lane E (lane I / whoever owns them)

At the time of writing, `cargo clippy -p ownhealth-api --no-deps -- -D warnings` fails in
`routes/observations.rs:563` (`map_entry`), and a later state of the tree did not build:
`routes/core.rs` imports `ownhealth_mcp` (not a dependency of ownhealth-api) and `crate::validate`
(no such module). Lane E's `routes/status.rs`, `routes/drug_check.rs` and `tests/status.rs` built,
passed (7/7) and were clippy clean before that change.
