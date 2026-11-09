# Lane A (ownhealth-normalize): shared changes

## Done in server/Cargo.toml (RUST-CONTRACTS.md section 2 allows one sorted line per dependency)

- `[workspace.dependencies]` data section: `num-bigint = "0.4"`, `num-rational = "0.4"`, `num-traits = "0.2"`,
  `unicode-normalization = "0.1"`. All four were already in Cargo.lock as transitive dependencies; they give
  exact unit conversion (Python's `Fraction`) and NFKD/NFKC folding (Python's `unicodedata`).
- `[workspace.dependencies]` tests section: `proptest = { version = "1.8", default-features = false, features = ["std"] }`
  for the Hypothesis property tests of `tests/normalize/`. New in Cargo.lock (proptest and its small deps).

## Signature notes (RUST-CONTRACTS.md section 4.2)

- `derivation_version()` now returns `String`, not `Result<String>`: the tables are embedded, so it cannot fail.
  No other crate called it.
- `group_units(group)` keeps `Option<Vec<(String, String)>>`; the pairs are `("canonical", ..)`, `("si", ..)`,
  `("conventional", ..)` in that order, as Python's dict. Lane E's rendering already reads it that way.
- New public items other lanes may use: `derive(conn, &DeriveIn)` (the derived layer of one row), `Derived`,
  `fraction_to_decimal`, and the modules `tables` (loinc_codes, groups, molar, aliases, fold), `units`,
  `text` (py_repr, looks_like_range, printed_flag_code) and `resolve` (unit_fits, effective_unit, ...).
