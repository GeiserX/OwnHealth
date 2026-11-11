# HOMA-IR

**Source.** Matthews DR et al. Homeostasis model assessment: insulin resistance and beta-cell function
from fasting plasma glucose and insulin concentrations in man. Diabetologia 1985;28:412-419.
https://pubmed.ncbi.nlm.nih.gov/3899825/

**Formula.**

```text
HOMA-IR = fasting glucose [mmol/L] x fasting insulin [µU/mL] / 22.5
```

**Units.**

- Glucose in mg/dL is divided by 18 (D-glucose is 180.16 g/mol). This gives the familiar mg/dL form
  glucose × insulin / 405.
- Insulin in mIU/L equals µIU/mL.
- Insulin in pmol/L is divided by 6.00. Knopp JL et al. (J Diabetes Sci Technol 2019,
  https://pubmed.ncbi.nlm.nih.gov/30318910/) show this is the correct factor. Many labs and
  calculators use 6.945 instead. With that factor the result would be about 14% lower. Every result
  that converted pmol/L carries the limitation `insulin_factor`, which says so.

**Validity.** Glucose and insulin must come from the same draw (`inputs_not_same_draw`). The record
does not say whether a draw was fasting, so every result carries the limitation
`fasting_not_recorded`. No published cutoff is shown.

**Precision.** 2 decimals.

**Worked example.** Glucose 5.0 mmol/L and insulin 10 µU/mL: 50 / 22.5 = 2.22.

**Code.** `server/crates/ownhealth-rules/src/calc/homa_ir.rs`. Vectors: `tests/fixtures/calc/homa_ir.json`.
