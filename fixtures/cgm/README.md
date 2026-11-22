# CGM export fixtures (LibreView and Dexcom Clarity)

Synthetic files for the CSV importer (`ownhealth-devices/src/cgm_csv.rs`). The person is Lucía Rivera of
the synthetic demo family and every value is invented. No real export went into these files.

The layout follows each vendor's published export: the header row, the record or event types, the
metadata, the date formats and the units.

| File | Vendor | Language | Unit | What it covers |
|---|---|---|---|---|
| `libreview-en-mgdl.csv` | LibreView | en | mg/dL | metadata line, BOM, CRLF, month-first dates with a 12-hour clock, record types 0 and 1 read, food, insulin and note rows left out, 69/70 and 180/181 mg/dL |
| `libreview-de-mmol.csv` | LibreView | de | mmol/L | day-first dates, 3.8/3.9 and 10.0/10.1 mmol/L, a strip glucose row left out |
| `libreview-es-mgdl.csv` | LibreView | es | mg/dL | day-first dates with no day above 12 (the dates' span decides the order), one column the importer does not know |
| `clarity-en-mgdl.csv` | Dexcom Clarity | en | mg/dL | the header block (name, birth date, device, alert thresholds), EGV rows, `Low` and `High`, a calibration and an insulin event left out |
| `clarity-de-mmol.csv` | Dexcom Clarity | de | mmol/L | semicolons, every cell quoted, decimal commas, `Niedrig` and `Hoch` |
| `clarity-es-mmol.csv` | Dexcom Clarity | es | mmol/L | decimal commas in quoted cells, `Bajo` and `Alto` |

## Where the headers come from

- LibreView, English: the "Download glucose data" export (`Glucose Data,Generated on,...` and then
  `Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL,...`).
- LibreView, German and Spanish: the same export with the account in that language
  (`Gerätezeitstempel`, `Aufzeichnungstyp`, `Glukosewert-Verlauf`; `Sello de tiempo del dispositivo`,
  `Tipo de registro`, `Historial de glucosa`), as the open-source LibreView readers document them.
- Dexcom Clarity, English: the CSV export
  (`Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,...,Glucose Value (mg/dL),...,Transmitter ID`).
- Dexcom Clarity, German: the export of a German account, with semicolons and quoted cells
  (`Zeitstempel (JJJJ-MM-TTThh:mm:ss)`, `Ereignisart`, `Glukosewert (mg/dL)`, `Niedrig`, `Hoch`).
- Dexcom Clarity, Spanish: no published sample was found. The header follows Dexcom's Spanish labels
  (`Marca de tiempo (AAAA-MM-DDThh:mm:ss)`, `Tipo de evento`, `Valor de glucosa`). The importer finds the
  timestamp column by its `...Thh:mm:ss)` pattern and the glucose column by its unit, so a different
  wording of those two columns still reads. A real Spanish export should replace this file.

The columns a file holds that the importer does not read (insulin, carbohydrates, notes, ketones) are
known to it by these names. A column it does not know is reported in the import result.
