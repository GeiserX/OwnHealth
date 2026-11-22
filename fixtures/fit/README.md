# FIT activity files (synthetic)

Activities of Lucía Rivera, the synthetic demo person, for the FIT importer (`ownhealth_devices::fit`, v1.0 slice 29) and its browser checks (`ui/e2e/fit_import.spec.ts`).

The small FIT writer in `server/crates/ownhealth-devices/tests/fit_writer/` writes every file here. It follows the public FIT protocol (header, definition and data messages, CRC-16) and uses the message and field numbers of the public FIT profile. No file comes from a device, a vendor SDK or a person. The test `the_committed_fixtures_are_what_the_writer_makes` fails when a file differs from what the writer makes. `OWNHEALTH_WRITE_FIT_FIXTURES=1 cargo test -p ownhealth-devices --test fit the_committed_fixtures` writes them again.

| File | What it holds |
|---|---|
| `lucia-run-gps.fit` | a 30-minute run on 2026-11-14 at 08:00 UTC: a record every 5 s with heart rate and a position, 5.23 km, 312 kcal |
| `lucia-ride.fit.gz` | a gzipped 20-minute bike ride on 2026-11-15 at 17:30 UTC: heart rate every 10 s, 8.4 km, 210 kcal, no positions |
| `lucia-run-cut.fit` | the first half of the run: a file cut short |
| `lucia-course.fit` | a course: positions only, no heart rate, distance or energy |

The positions are points in a public park, not an address.
