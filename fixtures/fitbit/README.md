# Fitbit Takeout fixtures

Synthetic exports of Lucía Rivera, the demo family's daughter. No value comes from a real person.

The folder, file and column names copy real Google Takeout exports:

- `legacy.zip`: the Fitbit layout under `Takeout/Fitbit/`.
  - `Your Profile/Profile.csv`, with Fitbit's column list. Only `timezone` is read.
  - `Global Export Data/heart_rate-*.json` and `resting_heart_rate-*.json`. Heart rate times are UTC.
  - `Heart Rate Variability/Heart Rate Variability Details - *.csv`, in local time without an offset.
  - `Oxygen Saturation (SpO2)/Daily SpO2 - *.csv` and `Minute SpO2 - *.csv`. The minute file is listed as not imported.
- `google-data.zip`: the Google Health layout under `Takeout/Google Health/`, with one folder per series in `Physical Activity_GoogleData/`. Times are UTC with `Z`. A daily series writes its calendar day as midnight UTC.
- `unrecognized.zip`: a Fitbit folder with no file the import reads.

Sources for the layout:

- The README files Fitbit puts in each export folder (`User Profile README.txt`, `Daily SpO2 README.txt`, `Heart Rate Variability README.txt`), as kept with a public export at https://github.com/vic-goncalves/t1d_data/tree/6616b0177c01adddd3daab96dbb81aeae289de7d/data/used_data/fitbit/Takeout/Fitbit
- That export's `heart_rate-2023-10-13.json` starts at 04:00 for an account in America/New_York, which is local midnight in UTC.
- The Google Health folder tree and the UTC and daily-date rules: https://github.com/blakster/air-health/blob/76171a367b1095571b4d20cd61809e5025fbb797/docs/takeout-format.md

The value column names of the Google Health HRV and SpO2 CSVs are not confirmed by a real export yet. The importer accepts several names for them.

Each archive brings four readings. `the_committed_fixtures_import_four_readings_each` in `server/crates/ownhealth-devices/tests/takeout_fitbit.rs` pins them, and `ui/e2e/fitbit_import.spec.ts` uploads them.
