# Gadgetbridge

Import the data of a band, watch or scale that Gadgetbridge manages on an Android phone: steps, heart rate, resting heart rate, SpO2, breathing rate and weight. It all stays offline: you export Gadgetbridge's database file and upload it to OwnHealth.

## 1. Export the database in Gadgetbridge

In Gadgetbridge, open **Data management** from the menu (older versions call it **Database management**) and tap **Export DB**. Gadgetbridge writes a file named `Gadgetbridge`, with no extension, to the export folder it shows on that screen. Copy it off the phone if you upload from a computer. Upload the database file itself, not a zip export.

## 2. Upload it to OwnHealth

In OwnHealth, open **Import & review** on the profile the data belongs to, tap **Upload** and pick the file. On a phone, the picker shows files with no extension too. On a computer you can also drag the file onto the upload panel. OwnHealth recognises the database by its content, not by its name.

The file becomes a document of that profile and owns the readings it brought. Retracting the document removes them.

Each export holds everything Gadgetbridge has, so upload a new one whenever you like. The newest upload owns every reading it holds: retracting an older export keeps what a newer one still has. Retracting the newest removes those readings too, until you upload it again.

## What is imported

| Gadgetbridge data | OwnHealth metric |
|---|---|
| Activity samples (steps per minute, heart rate) | steps, heart rate |
| Heart rate samples | heart rate |
| Resting heart rate | resting heart rate |
| SpO2 | SpO2 |
| Breathing rate | respiratory rate |
| Weight (Mi Scale and other scales) | weight |

Gadgetbridge writes 0 or 255 where a device measured nothing; OwnHealth drops those and other values outside a plausible range, and readings dated before 2000 or in the future (a band whose clock was never set writes 1970). The import summary counts them as `bad_time`. Sleep stages, stress, HRV and vendor scores are not imported yet. The import summary lists them by table.

The readings appear on the profile's **Wearables** page.

Gadgetbridge does not record the phone's time zone, so daily totals and resting heart rate from it fall on UTC days. In Madrid, steps taken between midnight and 01:00 (02:00 in summer) count to the day before.
