# Calculators

OwnHealth runs a few published formulas on the lab values a person has recorded. Each result shows its
formula version, its citation and the values it used, each with its source. When a result cannot be
computed, it says why. This is information, not diagnosis.

| Calculator | Page | Inputs (one draw) | Validity |
|---|---|---|---|
| eGFR, CKD-EPI 2021 creatinine | [ckd-epi-2021.md](ckd-epi-2021.md) | creatinine | adults (18 and over), sex at birth recorded |
| eGFR, CKD-EPI 2021 creatinine-cystatin C | [ckd-epi-2021.md](ckd-epi-2021.md) | creatinine, cystatin C | as above |
| FIB-4 | [fib-4.md](fib-4.md) | AST, ALT, platelets | age 35 and over |
| HOMA-IR | [homa-ir.md](homa-ir.md) | glucose, insulin | same draw; fasting is not recorded |

## How a result is made

- **Computed on read.** Nothing is stored. Each read builds the document `ownhealth.calculators/v1`
  for one profile and one `as_of` day. Its `calculators_hash` is the sha256 of the same canonical JSON
  that `state_hash` uses. The document never enters the hashed state body.
- **One draw.** All inputs of one result come from one draw: the lab report (`report_key`), else the
  document, else the day. The timeline groups a draw the same way. A formula runs once for each draw
  that holds all its inputs, so the kidney card can show eGFR over time.
- **Units.** `data/calculators/units.yaml` lists each unit a formula accepts and the number the
  printed value is divided by, with its citation. A unit with no row is refused (`unit_unsupported`).
  No factor is guessed.
- **Arithmetic.** Decimal arithmetic (`rust_decimal`, with exp and ln), never floating point. The value
  is rounded to the precision in `data/calculators/calculators.yaml`.
- **Refusals.** A result that fails a validity rule has status `refused`. A missing input gives
  `not_assessed`. Neither carries a value. The reason codes are `missing_input`,
  `inputs_not_same_draw`, `not_adult`, `age_below_35`, `sex_not_recorded`,
  `birth_date_not_recorded`, `unit_unsupported`, `not_a_number` (a value printed as `< 0.3`, or text)
  and `out_of_range`.

## Where to read it

- REST: `GET /api/profiles/{p}/calculators?as_of=&lang=`, with scope `read:labs`.
- MCP: the resource `ownhealth://profile/{id}/calculators`, with scope `read:labs`. There is no tool.
- UI: the Calculators card on a biomarker page lists every calculator that reads that marker. The
  Related markers card of a kidney or liver marker also shows the computed eGFR or FIB-4.

## How the numbers are checked

`server/crates/ownhealth-rules/tests/fixtures/calc/*.json` holds vectors. Each file names its source:
the R package kidney.epi 1.4.0 for CKD-EPI 2021, and the paper's formula for FIB-4 and HOMA-IR. Every
unit the units file accepts has at least one vector. `tests/calculators.rs` checks every vector to 2
decimals, and a control proves that one wrong expected value fails the suite.
