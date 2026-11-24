# C-CDA documents (HL7 samples)

Sample C-CDA documents for the C-CDA importer (`ownhealth_ingest_structured::ccda`, v1.0 slice 20) and its tests (`server/crates/ownhealth-ingest-structured/tests/ccda.rs`).

Every file is a copy, unchanged, of a sample in the HL7 C-CDA-Examples repository (https://github.com/HL7/C-CDA-Examples, commit `ad5007abd912a45bbd04ce96e871c70954c2b2c2`). HL7 publishes those samples under CC0 1.0 Universal (public domain dedication). Each sample describes a fictional person; none is a real patient. The tests read these files from disk and never fetch anything.

| File | HL7 sample |
|---|---|
| `history-and-physical.xml` | `Documents/History and Physical/History_and_Physical.xml`: all six core sections and eleven others |
| `ccd-1.xml` | `Documents/CCD/CCD 1/CCD.XML`: a CCD whose allergy concern act carries its own Severity Observation |
| `ccd-2.xml` | `Documents/CCD/CCD 2/CCD.xml`: a CCD whose Medications and Problems sections say "no information" |
| `sections/allergies-none-known.xml` | `Allergies/No Known Allergies/No Known Allergies with Author Timestamp(C-CDA2.1).xml` |
| `sections/allergies-not-peanuts.xml` | `Allergies/Not Allergic to Peanuts/Not Allergic to Peanuts(C-CDAR2.1).xml` |
| `sections/problems-none-known.xml` | `Problems/No Known Problems/No Known Problems(C-CDA2.1).xml` |
| `sections/problems-not-diabetes.xml` | `Problems/Patient Does Not Have Diabetes/Patient Does Not Have Diabetes(C-CDA2.1).xml` |
| `sections/medications-none.xml` | `Medications/No Medications/No Medications(C-CDA2.1).xml` |
| `sections/immunization-refused.xml` | `Immunizations/Immunization not given Patient refused/Immunization not given Patient refused(C-CDA2.1).xml` |
| `sections/results-less-than.xml` | `Results/Results with less than specific value/Results with less than specific value(C-CDA2.1).xml` |

The files under `sections/` hold one section each, not a whole document. The tests put each one inside a minimal `ClinicalDocument`.
