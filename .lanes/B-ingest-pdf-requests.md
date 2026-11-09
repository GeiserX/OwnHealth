# Lane B (ownhealth-ingest-pdf): shared changes and requests

## Done in shared files (RUST-CONTRACTS.md section 2: one sorted line per dependency)

- `server/Cargo.toml` `[workspace.dependencies]`: `flate2 = "1.1"` (PNG page renders and the image
  stream of the synthetic scan; already in Cargo.lock through other crates). `unicode-normalization`
  was already added by another lane; lane B uses it too.
- `server/crates/ownhealth-ingest-pdf/Cargo.toml` dev-dependency on `ownhealth-ingest-structured`
  (lane C's `retract_document_facts` in the import tests; C does not depend on B, so no cycle).

## Requests to the foundation

1. **Wire lane C's retract into lane B at start-up.** `assign_document` (moving a document to another
   profile, the `document_identity` accept handler and `POST /api/documents/{id}/assign`) calls
   `retract_document_facts`, which lives in lane C, and the dependency graph does not let B depend on
   C. Lane B takes it as a hook. Add one line to `crates/ownhealth/src/lib.rs` next to
   `ownhealth_ingest_pdf::register()`:

   ```rust
   ownhealth_ingest_pdf::set_document_retract(ownhealth_ingest_structured::retract::retract_document_facts);
   ```

   Without it an assign answers 501 `not_implemented` ("lane C: retract_document_facts"). The
   signatures already match (`DocumentRetract`). The retract route in `ownhealth-api` calls lane C
   directly and needs nothing.
2. **libpdfium at run time.** The text layer and the page renders load PDFium from
   `OWNHEALTH_PDFIUM_LIB`, else `server/vendor/pdfium/<platform>/lib`, else the system library. The
   Dockerfile already copies it and sets the variable; CI fetches it with `scripts/fetch-pdfium.sh`.
   Nothing else to do, listed so it is not removed.

## Behaviour notes for the release notes / other lanes

- `extraction_run.engine` and the config hash now say PDFium: `engine = "pdfium"`,
  `engine_version = "chromium/8125"`, `config_hash = sha256("b-1|pdfiumchromium/8125|<template sha or generic>")`.
  0.1.1 rows keep their pdfplumber values; a forced re-import of a 0.1.1 document makes a new run
  (the hash differs), as a code-version change did before.
- The stored word layer (`document_page.words_blob`) says `"engine": "pdfium chromium/8125"`; its
  keys and word fields are unchanged.
- Word boxes: equal to pdfplumber's for the standard Helvetica fonts (AFM widths); for embedded
  TrueType fonts PDFium keeps integer glyph widths, so x1 can differ by up to 0.5 pt on long words.
  Text, font, size, colour, bold, hidden, page flags and every extracted row are equal on all 47
  synthetic slips.
- The PDF Info key `Trapped` is not readable through pdfium-render, so `flags.metadata_text` no
  longer carries it (the other 8 keys are kept).
- `ownhealth_ingest_pdf::Word` is `{text, bbox: [f64; 4], font, size, bold, hidden}`; lane F reads
  only `text`, `bbox`, `bold` and `hidden`.
- The committed injection fixtures (`fixtures/labslips/injection/`) are unchanged, byte for byte;
  `tests/textlayer.rs` checks that they read back the same as the Rust generator's output.
