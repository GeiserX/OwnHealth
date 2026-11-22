//! LibreView and Dexcom Clarity CSV exports through the real upload path: `dispatch_upload` sniffs the
//! file by its header row, the `import.cgm_csv` job stores the readings as `glucose_cgm` in mg/dL.
//! Synthetic exports for Lucía Rivera (fixtures/cgm, and files built here for the clock changes).

mod common;

use chrono::{DateTime, TimeZone, Utc};
use common::{Env, R};
use ownhealth_core::documents::{self, StoredFile};
use ownhealth_core::jobs;
use ownhealth_devices::cgm_csv;
use serde_json::{Value, json};
use uuid::Uuid;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/cgm");

fn fixture(name: &str) -> anyhow::Result<Vec<u8>> {
    Ok(std::fs::read(format!("{FIXTURES}/{name}"))?)
}

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s).single().unwrap_or_default()
}

/// Upload one file to a profile as the app does: sniff, enqueue. Returns (kind, job id).
async fn upload(env: &Env, slug: &str, name: &str, bytes: &[u8]) -> anyhow::Result<(String, Option<i64>)> {
    let sha = env.blobs.put_bytes(bytes)?;
    let mut c = env.db.pool.acquire().await?;
    let out = documents::dispatch_upload(
        &mut c,
        &env.blobs,
        env.fam.household_id,
        env.fam.profile(slug),
        &[StoredFile {
            filename: name.into(),
            sha256: sha,
            size: bytes.len() as i64,
        }],
        None,
    )
    .await?;
    let item = &out["items"][0];
    Ok((
        item["kind"].as_str().unwrap_or_default().to_string(),
        item["job_id"].as_i64(),
    ))
}

async fn job(env: &Env, id: Option<i64>) -> anyhow::Result<(String, Value, Option<String>)> {
    jobs::drain(&env.ctx, "core", 5, None).await?;
    Ok(
        sqlx::query_as("select state, coalesce(result, 'null'::jsonb), last_error from job where id = $1")
            .bind(id)
            .fetch_one(&env.db.pool)
            .await?,
    )
}

/// Upload a fixture to Lucía and run its import job.
async fn import(env: &Env, name: &str) -> anyhow::Result<(String, Value)> {
    let (kind, id) = upload(env, "lucia", name, &fixture(name)?).await?;
    let (state, result, err) = job(env, id).await?;
    assert_eq!(state, "done", "{name}: {err:?}");
    Ok((kind, result))
}

/// The CGM readings of a profile from the CSV importers: (time, mg/dL, offset in minutes).
async fn readings(env: &Env, slug: &str) -> anyhow::Result<Vec<(DateTime<Utc>, f32, Option<i16>)>> {
    Ok(sqlx::query_as(
        "select s.t, s.v, s.tz_offset_min from wearable_sample s join wearable_source w on w.id = s.source_id \
         where s.profile_id = $1 and s.metric = 'glucose_cgm' and w.provider in ('libreview', 'dexcom_clarity') \
         order by s.t",
    )
    .bind(env.fam.profile(slug))
    .fetch_all(&env.db.pool)
    .await?)
}

fn values(r: &[(DateTime<Utc>, f32, Option<i16>)]) -> Vec<f32> {
    r.iter().map(|x| x.1).collect()
}

fn at(r: &[(DateTime<Utc>, f32, Option<i16>)], t: DateTime<Utc>) -> Option<f32> {
    r.iter().find(|x| x.0 == t).map(|x| x.1)
}

#[tokio::test]
async fn every_fixture_is_sniffed_as_its_vendor() -> R {
    for (name, kind) in [
        ("libreview-en-mgdl.csv", "libreview_csv"),
        ("libreview-de-mmol.csv", "libreview_csv"),
        ("libreview-es-mgdl.csv", "libreview_csv"),
        ("clarity-en-mgdl.csv", "dexcom_clarity_csv"),
        ("clarity-de-mmol.csv", "dexcom_clarity_csv"),
        ("clarity-es-mmol.csv", "dexcom_clarity_csv"),
    ] {
        let bytes = fixture(name)?;
        let head = &bytes[..bytes.len().min(4096)];
        assert_eq!(documents::sniff(name, head, None), kind, "{name}");
    }
    Ok(())
}

#[tokio::test]
async fn a_libreview_export_brings_its_historic_and_scan_readings() -> R {
    let env = Env::new().await?;
    let (kind, result) = import(&env, "libreview-en-mgdl.csv").await?;
    assert_eq!(kind, "libreview_csv");
    let r = readings(&env, "lucia").await?;
    // record types 0 (12 historic readings) and 1 (one scan); food, insulin and a note are not readings
    assert_eq!(r.len(), 13, "{r:?}");
    assert_eq!(result["accepted"], json!(13), "{result}");
    assert_eq!(result["skipped"], json!(0), "{result}");
    assert_eq!(result["reasons"], json!({}), "{result}");
    let d = &result["cgm_csv"];
    assert_eq!(d["vendor"], json!("libreview"), "{result}");
    assert_eq!(d["unit"], json!("mg/dL"));
    assert_eq!(d["other_rows"], json!(3));
    assert_eq!(d["unknown_columns"], json!([]));
    assert_eq!(d["timezone"], Value::Null);
    assert_eq!(d["devices"], json!(["FreeStyle Libre 3"]));
    // month-first dates with a 12-hour clock; with no install time zone the wall clock is read as UTC
    // and no offset is claimed
    assert_eq!(r[0], (utc(2026, 11, 14, 10, 48, 0), 112.0, None));
    assert_eq!(at(&r, utc(2026, 11, 14, 12, 3, 0)), Some(154.0));
    assert_eq!(at(&r, utc(2026, 11, 14, 12, 10, 0)), Some(164.0)); // the scan
    assert_eq!(r[12], (utc(2026, 11, 14, 13, 33, 0), 109.0, None));
    // one wearable source per sensor, named by the export
    let src: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "select provider, device, external_id from wearable_source where profile_id = $1 and provider = 'libreview'",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_all(&env.db.pool)
    .await?;
    assert_eq!(
        src,
        vec![(
            "libreview".into(),
            Some("FreeStyle Libre 3".into()),
            Some("0B5A1C2D-7E3F-4A90-8C61-5D2E0F1A9C3B".into())
        )]
    );
    // the file is a document of the profile that owns its readings
    let (mime, imported, owned): (String, Value, i64) = sqlx::query_as(
        "select d.mime, d.flags->'import', (select count(*) from wearable_sample s where s.document_id = d.id) \
         from document d where d.profile_id = $1",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!(
        (mime.as_str(), imported["accepted"].clone(), owned),
        ("text/csv", json!(13), 13)
    );
    Ok(())
}

#[tokio::test]
async fn units_convert_exactly_at_the_clinical_thresholds() -> R {
    let env = Env::new().await?;
    // mg/dL is stored as printed: 69 and 181 stay outside 70-180, 70 and 180 stay inside
    import(&env, "libreview-en-mgdl.csv").await?;
    let mg = readings(&env, "lucia").await?;
    assert_eq!(at(&mg, utc(2026, 11, 14, 11, 18, 0)), Some(69.0));
    assert_eq!(at(&mg, utc(2026, 11, 14, 11, 33, 0)), Some(70.0));
    assert_eq!(at(&mg, utc(2026, 11, 14, 12, 18, 0)), Some(180.0));
    assert_eq!(at(&mg, utc(2026, 11, 14, 12, 33, 0)), Some(181.0));
    // mmol/L x 180.156 / 10, to the whole mg/dL a sensor measures: 3.8 -> 68 (low), 3.9 -> 70 (in
    // range), 10.0 -> 180 (in range, not 180.156), 10.1 -> 182 (high)
    let env = Env::new().await?;
    let (_, result) = import(&env, "libreview-de-mmol.csv").await?;
    assert_eq!(result["cgm_csv"]["unit"], json!("mmol/L"), "{result}");
    assert_eq!(result["cgm_csv"]["other_rows"], json!(1), "{result}"); // the strip glucose row
    let mm = readings(&env, "lucia").await?;
    assert_eq!(mm.len(), 13);
    assert_eq!(at(&mm, utc(2026, 11, 14, 6, 33, 0)), Some(68.0));
    assert_eq!(at(&mm, utc(2026, 11, 14, 6, 48, 0)), Some(70.0));
    assert_eq!(at(&mm, utc(2026, 11, 14, 7, 33, 0)), Some(180.0));
    assert_eq!(at(&mm, utc(2026, 11, 14, 7, 48, 0)), Some(182.0));
    assert_eq!(at(&mm, utc(2026, 11, 14, 7, 20, 0)), Some(133.0)); // the scan, 7.4
    let in_range = |v: f32| (70.0..=180.0).contains(&v);
    assert_eq!(
        [68.0, 70.0, 180.0, 182.0].map(in_range),
        [69.0, 70.0, 180.0, 181.0].map(in_range)
    );
    Ok(())
}

#[tokio::test]
async fn clarity_reads_egv_rows_only_and_counts_low_and_high() -> R {
    let env = Env::new().await?;
    let (kind, result) = import(&env, "clarity-en-mgdl.csv").await?;
    assert_eq!(kind, "dexcom_clarity_csv");
    let r = readings(&env, "lucia").await?;
    // the alert thresholds of the header block (250, 70, 55) and the calibration (118) are not readings
    assert_eq!(
        values(&r),
        vec![120.0, 69.0, 70.0, 88.0, 180.0, 181.0, 150.0, 133.0],
        "{r:?}"
    );
    assert_eq!(r[0].0, utc(2026, 11, 14, 20, 0, 17));
    assert_eq!(result["accepted"], json!(8), "{result}");
    assert_eq!(
        result["reasons"],
        json!({"below_sensor_range": 1, "above_sensor_range": 1}),
        "{result}"
    );
    assert_eq!(result["skipped"], json!(2));
    assert_eq!(result["cgm_csv"]["other_rows"], json!(2), "{result}");
    assert_eq!(result["cgm_csv"]["devices"], json!(["Dexcom G7"]));
    let src: (Option<String>, Option<String>) = sqlx::query_as(
        "select device, external_id from wearable_source where profile_id = $1 and provider = 'dexcom_clarity'",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!(src, (Some("Dexcom G7".into()), Some("Android G7".into())));

    // German: semicolons, quoted cells, decimal commas, Niedrig and Hoch
    let env = Env::new().await?;
    let (_, result) = import(&env, "clarity-de-mmol.csv").await?;
    assert_eq!(
        values(&readings(&env, "lucia").await?),
        vec![115.0, 68.0, 70.0, 180.0, 182.0, 139.0]
    );
    assert_eq!(
        result["reasons"],
        json!({"below_sensor_range": 1, "above_sensor_range": 1}),
        "{result}"
    );
    // Spanish: decimal commas in quoted cells, Bajo and Alto
    let env = Env::new().await?;
    let (_, result) = import(&env, "clarity-es-mmol.csv").await?;
    assert_eq!(values(&readings(&env, "lucia").await?), vec![106.0, 70.0, 180.0, 150.0]);
    assert_eq!(
        result["reasons"],
        json!({"below_sensor_range": 1, "above_sensor_range": 1}),
        "{result}"
    );
    assert_eq!(result["cgm_csv"]["devices"], json!(["Dexcom G6"]));
    Ok(())
}

#[tokio::test]
async fn a_column_the_importer_does_not_know_is_reported() -> R {
    let env = Env::new().await?;
    let (_, result) = import(&env, "libreview-es-mgdl.csv").await?;
    assert_eq!(
        result["cgm_csv"]["unknown_columns"],
        json!(["Glucosa del sensor adicional mg/dL"]),
        "{result}"
    );
    let r = readings(&env, "lucia").await?;
    // day-first: 1 and 2 November, not 11 January and 11 February
    assert_eq!(r.first().map(|x| x.0), Some(utc(2026, 11, 1, 23, 0, 0)));
    assert_eq!(r.last().map(|x| x.0), Some(utc(2026, 11, 2, 0, 15, 0)));
    // the unknown column's values are reported as a column, never read as readings
    assert_eq!(values(&r), vec![101.0, 97.0, 93.0, 90.0, 96.0, 104.0]);
    let flags: Value = sqlx::query_scalar("select flags->'cgm_csv' from document where profile_id = $1")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(flags["unknown_columns"], json!(["Glucosa del sensor adicional mg/dL"]));
    Ok(())
}

/// The English Clarity fixture with `more` EGV rows after its last reading, 5 minutes apart.
fn clarity_with_more(more: usize) -> anyhow::Result<Vec<u8>> {
    let mut text = String::from_utf8(fixture("clarity-en-mgdl.csv")?)?;
    for k in 0..more {
        text.push_str(&format!(
            "{},2026-11-14T21:{:02}:17,EGV,,,,Android G7,{},,,,,3459000,7Q2RLX\n",
            30 + k,
            5 * k,
            140 + k
        ));
    }
    Ok(text.into_bytes())
}

#[tokio::test]
async fn importing_the_same_export_again_adds_nothing() -> R {
    let env = Env::new().await?;
    import(&env, "clarity-en-mgdl.csv").await?;
    // the same file again: one document, no new reading, every one counted as already there
    let (state, result, err) = job(
        &env,
        upload(&env, "lucia", "again.csv", &fixture("clarity-en-mgdl.csv")?)
            .await?
            .1,
    )
    .await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["accepted"], json!(0), "{result}");
    assert_eq!(
        result["reasons"],
        json!({"duplicate": 8, "below_sensor_range": 1, "above_sensor_range": 1}),
        "{result}"
    );
    assert_eq!(readings(&env, "lucia").await?.len(), 8);
    let docs: Vec<Value> = sqlx::query_scalar("select flags->'import' from document where profile_id = $1")
        .bind(env.fam.profile("lucia"))
        .fetch_all(&env.db.pool)
        .await?;
    // one document; its import summary is still the first one
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["accepted"], json!(8));
    // a longer export that holds the first: only its new readings are added
    let (_, id) = upload(&env, "lucia", "longer.csv", &clarity_with_more(2)?).await?;
    let (state, result, err) = job(&env, id).await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["accepted"], json!(2), "{result}");
    assert_eq!(result["reasons"]["duplicate"], json!(8), "{result}");
    assert_eq!(readings(&env, "lucia").await?.len(), 10);
    Ok(())
}

#[tokio::test]
async fn retracting_one_of_two_overlapping_exports_keeps_what_the_other_holds() -> R {
    let env = Env::new().await?;
    import(&env, "clarity-en-mgdl.csv").await?;
    let (_, id) = upload(&env, "lucia", "longer.csv", &clarity_with_more(2)?).await?;
    assert_eq!(job(&env, id).await?.0, "done");
    let first: Uuid =
        sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'clarity-en-mgdl.csv'")
            .bind(env.fam.profile("lucia"))
            .fetch_one(&env.db.pool)
            .await?;
    let mut c = env.db.pool.acquire().await?;
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(first)
        .execute(&mut *c)
        .await?;
    ownhealth_ingest_structured::retract::retract_document_facts(&mut c, first, "wrong file").await?;
    drop(c);
    // the first file brought 8 readings and leaves with them; the longer one is imported again and
    // brings them back as its own
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let owners: Vec<(String, i64)> = sqlx::query_as(
        "select d.filename, count(*) from wearable_sample s join document d on d.id = s.document_id \
         where s.profile_id = $1 group by 1",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_all(&env.db.pool)
    .await?;
    assert_eq!(owners, vec![("longer.csv".to_string(), 10)]);
    Ok(())
}

/// A Clarity export with EGV rows at these local times (`YYYY-MM-DDThh:mm:ss`), 100 mg/dL each.
fn clarity_at(times: &[String]) -> Vec<u8> {
    let mut text = String::from(
        "Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,Event Subtype,Patient Info,Device Info,Source Device ID,\
         Glucose Value (mg/dL),Insulin Value (u),Carb Value (grams),Duration (hh:mm:ss),\
         Glucose Rate of Change (mg/dL/min),Transmitter Time (Long Integer),Transmitter ID\n",
    );
    for (i, t) in times.iter().enumerate() {
        text.push_str(&format!(
            "{},{t},EGV,,,,iOS G7,100,,,,,{},8M4TQZ\n",
            i + 1,
            9_000_000 + 300 * i
        ));
    }
    text.into_bytes()
}

/// Every 5 minutes from `h0:00` to `h1:55` local on one day.
fn every_5(day: &str, h0: u32, h1: u32) -> Vec<String> {
    (h0 * 60..(h1 + 1) * 60)
        .step_by(5)
        .map(|m| format!("{day}T{:02}:{:02}:00", m / 60, m % 60))
        .collect()
}

/// Store the bytes as a document of `slug` and import it with the given install time zone.
async fn import_in_zone(env: &Env, slug: &str, bytes: &[u8], zone: Option<&str>) -> anyhow::Result<Value> {
    let sha = env.blobs.put_bytes(bytes)?;
    let mut c = env.db.pool.acquire().await?;
    let payload = json!({
        "profile_id": env.fam.profile(slug).to_string(),
        "household_id": env.fam.household_id.to_string(),
        "blob_sha256": sha,
        "filename": "clarity.csv",
    });
    let doc = ownhealth_ingest_structured::jobs::document_for(&mut c, &env.blobs, &payload, "text/csv", "upload")
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let (summary, _) = cgm_csv::import_cgm_csv(&mut c, &env.blobs, doc, None, zone).await?;
    Ok(summary.to_value())
}

#[tokio::test]
async fn local_times_are_read_in_the_install_zone_across_a_clock_change() -> R {
    let env = Env::new().await?;
    // 25 October 2026 in Madrid: at 03:00 CEST the clocks go back to 02:00 CET, so the device writes
    // 02:00 to 02:55 twice. 29 March 2026: at 02:00 CET they go forward to 03:00 CEST, and a reading
    // at 02:30 cannot be a Madrid time.
    let mut times = every_5("2026-10-25", 0, 2);
    times.extend(every_5("2026-10-25", 2, 3));
    times.extend(["01:50", "01:55", "02:30", "03:00", "03:05"].map(|t| format!("2026-03-29T{t}:00")));
    let bytes = clarity_at(&times);

    let result = import_in_zone(&env, "lucia", &bytes, Some("Europe/Madrid")).await?;
    let r = readings(&env, "lucia").await?;
    // the repeated hour cannot be told apart: its 24 rows are counted, not guessed
    assert_eq!(
        result["reasons"],
        json!({"ambiguous_time": 24, "nonexistent_time": 1}),
        "{result}"
    );
    assert_eq!(result["accepted"], json!(24 + 12 + 4));
    let find = |t: DateTime<Utc>| r.iter().find(|x| x.0 == t).map(|x| x.2);
    // CEST is UTC+2 before the change, CET UTC+1 after it; each reading keeps the offset that applied
    assert_eq!(find(utc(2026, 10, 24, 22, 0, 0)), Some(Some(120))); // 00:00 local
    assert_eq!(find(utc(2026, 10, 24, 23, 55, 0)), Some(Some(120))); // 01:55 local
    assert_eq!(find(utc(2026, 10, 25, 2, 0, 0)), Some(Some(60))); // 03:00 local
    assert_eq!(find(utc(2026, 10, 25, 2, 55, 0)), Some(Some(60))); // 03:55 local
    assert_eq!(find(utc(2026, 3, 29, 0, 55, 0)), Some(Some(60))); // 01:55 CET
    assert_eq!(find(utc(2026, 3, 29, 1, 0, 0)), Some(Some(120))); // 03:00 CEST
    assert_eq!(r.len(), 40);

    // with no install zone the wall clock is read as UTC, as the other importers read a time without an
    // offset: no offset is claimed, and the second pass of the repeated hour lands on the first
    let result = import_in_zone(&env, "sam", &bytes, None).await?;
    let r = readings(&env, "sam").await?;
    assert_eq!(result["reasons"], json!({"repeated_time": 12}), "{result}");
    assert_eq!(r.len(), 24 + 12 + 12 + 5);
    assert!(r.iter().all(|x| x.2.is_none()), "{r:?}");
    assert_eq!(r[0].0, utc(2026, 3, 29, 1, 50, 0));

    // a zone Postgres does not know is no zone
    let mut c = env.db.pool.acquire().await?;
    assert_eq!(cgm_csv::known_zone(&mut c, Some("Mars/Olympus_Mons")).await?, None);
    assert_eq!(cgm_csv::known_zone(&mut c, Some("")).await?, None);
    assert_eq!(cgm_csv::known_zone(&mut c, None).await?, None);
    assert_eq!(
        cgm_csv::known_zone(&mut c, Some("Europe/Madrid")).await?.as_deref(),
        Some("Europe/Madrid")
    );
    Ok(())
}

#[tokio::test]
async fn an_export_without_a_glucose_column_fails_for_good_and_writes_nothing() -> R {
    let env = Env::new().await?;
    let bytes = b"Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type\n1,2026-11-14T20:00:00,EGV\n";
    let (kind, id) = upload(&env, "lucia", "short.csv", bytes).await?;
    assert_eq!(kind, "dexcom_clarity_csv");
    let (state, _, err) = job(&env, id).await?;
    assert_eq!(state, "dead");
    assert!(err.unwrap_or_default().contains("no glucose column"));
    assert_eq!(
        env.count(
            "select count(*) from wearable_sample where profile_id = $1",
            env.fam.profile("lucia")
        )
        .await?,
        0
    );
    Ok(())
}
