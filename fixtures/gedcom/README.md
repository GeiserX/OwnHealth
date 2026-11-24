# GEDCOM fixtures

Synthetic family trees for the GEDCOM import (`server/crates/ownhealth-core/src/gedcom.rs`). Nobody in
them exists.

- `rivera-551.ged`: a GEDCOM 5.5.1 tree of the Rivera family around Alex (position 0): parents,
  grandparents, a sister, an adopted brother, a half-brother, a partner, a daughter, an uncle, a cousin,
  a nephew and a father-in-law, plus relatives by marriage the catalog has no code for.
- `rivera-70.ged`: a GEDCOM 7.0 file in the shape of the gedcom.io `maximal70.ged` test file: the same
  records, levels, tags, pointers, dates and enumerations, with every free-text value replaced by a
  synthetic one.

Every NAME holds `Vexlorn`, every NOTE, CONT and other free text `Zirkhaven`, and every PLAC `Quillmoor`.
`tests/gedcom_never_stored.rs` checks that none of them is stored or logged.
