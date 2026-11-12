# eGFR: CKD-EPI 2021

**Source.** Inker LA et al. New creatinine- and cystatin C-based equations to estimate GFR without
race. N Engl J Med 2021;385:1737-1749. https://pubmed.ncbi.nlm.nih.gov/34554658/

**Formulas.** Creatinine (Scr) is in mg/dL, cystatin C (Scys) in mg/L, and age in whole years at the
draw. The result is in mL/min/1.73 m².

```text
creatinine:              142 x min(Scr/k, 1)^a x max(Scr/k, 1)^-1.200 x 0.9938^age  [x 1.012 if female]
creatinine-cystatin C:   135 x min(Scr/k, 1)^a x max(Scr/k, 1)^-0.544
                             x min(Scys/0.8, 1)^-0.323 x max(Scys/0.8, 1)^-0.778
                             x 0.9961^age  [x 0.963 if female]

k = 0.7 (female), 0.9 (male)
a = -0.241 / -0.302 (creatinine), -0.219 / -0.144 (creatinine-cystatin C), female / male
```

**Units.** Creatinine in µmol/L is divided by 88.4 (creatinine is 113.12 g/mol). Cystatin C must be in
mg/L.

**Validity.**

- Adults only: age 18 or over at the draw (`not_adult`).
- Sex at birth must be recorded as female or male (`sex_not_recorded`).
- Creatinine and cystatin C must come from the same draw. If the record has creatinine but no
  cystatin C, the creatinine result shows and the combined one says `missing_input`.

**Precision.** Shown as a whole number. The vectors check it to 2 decimals.

**Worked example.** A man aged 47 with creatinine 0.98 mg/dL: 95.71, shown as 96. The same man with
cystatin C 0.85 mg/L in the same draw: 102.32. Both values come from kidney.epi 1.4.0.

**Code.** `server/crates/ownhealth-rules/src/calc/ckd_epi_2021.rs`. Vectors:
`tests/fixtures/calc/ckd_epi_2021_cr.json` and `ckd_epi_2021_cr_cys.json`.
