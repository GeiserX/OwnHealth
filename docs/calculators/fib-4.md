# FIB-4

**Source.** Sterling RK et al. Development of a simple noninvasive index to predict significant
fibrosis in patients with HIV/HCV coinfection. Hepatology 2006;43:1317-1325.
https://pubmed.ncbi.nlm.nih.gov/16729309/

**Formula.**

```text
FIB-4 = age [years] x AST [U/L] / (platelets [10^9/L] x sqrt(ALT [U/L]))
```

**Units.** AST and ALT in U/L. Platelets in 10^9/L or 10^3/µL, which is the same number.

**Published bands.** These are shown as published categories, not as a diagnosis.

- 1.3 and 2.67: Shah AG et al., Clin Gastroenterol Hepatol 2009. https://pubmed.ncbi.nlm.nih.gov/19523535/
- From age 65 the lower cutoff is 2.0: McPherson S et al., Am J Gastroenterol 2017.
  https://pubmed.ncbi.nlm.nih.gov/27725647/ Such a result also carries the limitation `fib4_age_65`.

**Validity.** Age 35 or over at the draw (`age_below_35`). AST, ALT and platelets must come from the
same draw.

**Precision.** 2 decimals.

**Worked example.** Age 35, AST 30 U/L, ALT 25 U/L, platelets 220 × 10^9/L:
35 × 30 / (220 × 5) = 0.95, below the published lower cutoff (1.3).

**Code.** `server/crates/ownhealth-rules/src/calc/fib4.rs`. Vectors: `tests/fixtures/calc/fib4.json`.
The paper gives no worked example, so we evaluated its formula once with Python decimal.
