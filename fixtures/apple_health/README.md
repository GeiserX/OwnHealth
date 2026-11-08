# Apple Health fixtures

Exports are not checked in. Tests build them with
`ownhealth.ingest.apple_health.synthetic.write_export`. Its layout follows a real export: `export.xml`
with its DTD header, `<Me>`, `Record` elements with an escaped `device` string, `clinical-records/*.json`
and `electrocardiograms/*.csv`. The people are the synthetic Rivera family and the values are invented.

## Memory on a large export

Measured on 2026-11-08 on the MacMini (Python 3.13, Postgres 17.11 in Docker), with
`backend/tests/imports/measure_memory.py`. The synthetic export has a Watch heart rate every minute,
and each mode runs in its own process.

| Mode | export.xml | Samples | Time | Peak RSS |
|---|---|---|---|---|
| parse only | 22 MB | 59,046 | 1.2 s | 53 MB |
| parse only | 224 MB | 590,458 | 8.9 s | 54 MB |
| full import into Postgres | 22 MB | 59,046 | 1.8 s | 69 MB |
| full import into Postgres | 224 MB | 590,458 | 17.9 s | 68 MB |

The process starts at about 53 MB, so the importer adds about 15 MB, and that does not grow with the file.
Samples go to Postgres through COPY in batches of 10,000, and the XML parser drops each element once it is
read. Extrapolated, a 4 GB export (about ten years of Apple Watch data) should still peak near 70 MB in
the app and take roughly 5 to 6 minutes. That is an estimate, not a measurement. The staging table and the
rollup queries grow on the Postgres side, on disk, not in the app.
