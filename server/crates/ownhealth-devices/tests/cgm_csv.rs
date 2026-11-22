//! LibreView and Dexcom Clarity CSV exports through the real upload path: `dispatch_upload` sniffs the
//! file by its header row, the `import.cgm_csv` job stores the readings as `glucose_cgm` in mg/dL.
//! Synthetic exports for Lucía Rivera (fixtures/cgm, and files built here for the clock changes).

mod common;

use chrono::{DateTime, TimeZone, Utc};
use common::{Env, R};
use ownhealth_core::blobs::BlobOwner;
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
    let sha = env.blobs.put_bytes(BlobOwner::Profile(env.fam.profile(slug)), bytes)?;
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
    let held: Value = sqlx::query_scalar(
        "select flags->'cgm_csv'->'held' from document where profile_id = $1 and filename = 'longer.csv'",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!(held, json!(2));
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
    // the retraction cleared its import summary, so it was imported anew: its document screen counts
    // the 10 it holds now, 8 of them brought by this run
    let flags: Value =
        sqlx::query_scalar("select flags from document where profile_id = $1 and filename = 'longer.csv'")
            .bind(env.fam.profile("lucia"))
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(flags["cgm_csv"]["held"], json!(10), "{flags}");
    assert_eq!(flags["import"]["accepted"], json!(8), "{flags}");
    Ok(())
}

#[tokio::test]
async fn a_file_imported_again_counts_the_readings_it_holds_now() -> R {
    let env = Env::new().await?;
    import(&env, "clarity-en-mgdl.csv").await?;
    let longer = clarity_with_more(2)?;
    let (_, id) = upload(&env, "lucia", "longer.csv", &longer).await?;
    assert_eq!(job(&env, id).await?.0, "done");
    let doc = |name: &'static str| {
        sqlx::query_scalar::<_, Uuid>("select id from document where profile_id = $1 and filename = $2")
            .bind(env.fam.profile("lucia"))
            .bind(name)
            .fetch_one(&env.db.pool)
    };
    let (first, other) = (doc("clarity-en-mgdl.csv").await?, doc("longer.csv").await?);
    // the first file's readings pass to the longer one while its import summary stays (as another
    // writer could do); the same file uploaded again is an import again, and still counts what it holds
    sqlx::query("update wearable_sample set document_id = $2 where document_id = $1")
        .bind(first)
        .bind(other)
        .execute(&env.db.pool)
        .await?;
    let (_, id) = upload(&env, "lucia", "longer.csv", &longer).await?;
    let (_, result, _) = job(&env, id).await?;
    assert_eq!(result["cgm_csv"]["again"], json!(true), "{result}");
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(other)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(flags["cgm_csv"]["held"], json!(10), "{flags}");
    // the summary of its first import stays
    assert_eq!(flags["cgm_csv"]["readings"], json!(2), "{flags}");
    assert_eq!(flags["import"]["accepted"], json!(2), "{flags}");
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
    let sha = env.blobs.put_bytes(BlobOwner::Profile(env.fam.profile(slug)), bytes)?;
    let mut c = env.db.pool.acquire().await?;
    let payload = json!({
        "profile_id": env.fam.profile(slug).to_string(),
        "household_id": env.fam.household_id.to_string(),
        "blob_sha256": sha,
        "filename": "clarity.csv",
    });
    let doc = ownhealth_ingest_structured::jobs::document_of_type(
        &mut c,
        &env.blobs,
        &payload,
        "text/csv",
        "upload",
        documents::WEARABLES_DOC_TYPE,
    )
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
    // the old names Chromium reports read as their current ones, which Postgres knows
    for (old, current) in [
        ("Asia/Calcutta", "Asia/Kolkata"),
        ("America/Buenos_Aires", "America/Argentina/Buenos_Aires"),
        ("Europe/Kiev", "Europe/Kyiv"),
        ("Asia/Saigon", "Asia/Ho_Chi_Minh"),
        ("Asia/Katmandu", "Asia/Kathmandu"),
        ("America/Indianapolis", "America/Indiana/Indianapolis"),
        ("Atlantic/Faeroe", "Atlantic/Faroe"),
    ] {
        assert_eq!(
            cgm_csv::known_zone(&mut c, Some(old)).await?.as_deref(),
            Some(current),
            "{old}"
        );
    }
    assert_eq!(cgm_csv::known_zone(&mut c, Some(&"A".repeat(65))).await?, None);
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

/// A Clarity export with these EGV rows: (local time, source device id, mg/dL).
fn clarity_rows(rows: &[(String, String, u32)]) -> Vec<u8> {
    let mut text = String::from(
        "Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,Event Subtype,Patient Info,Device Info,Source Device ID,\
         Glucose Value (mg/dL),Insulin Value (u),Carb Value (grams),Duration (hh:mm:ss),\
         Glucose Rate of Change (mg/dL/min),Transmitter Time (Long Integer),Transmitter ID\n",
    );
    for (i, (t, dev, v)) in rows.iter().enumerate() {
        text.push_str(&format!(
            "{},{t},EGV,,,,{dev},{v},,,,,{},8M4TQZ\n",
            i + 1,
            9_000_000 + 300 * i
        ));
    }
    text.into_bytes()
}

/// The install-wide wearable source counter: (last value, called).
async fn source_counter(env: &Env) -> anyhow::Result<(i64, bool)> {
    Ok(
        sqlx::query_as("select last_value, is_called from wearable_source_id_seq")
            .fetch_one(&env.db.pool)
            .await?,
    )
}

#[tokio::test]
async fn a_file_that_names_too_many_sensors_is_refused_before_it_writes() -> R {
    let env = Env::new().await?;
    // every reading from its own "sensor": each would take a wearable source id of the whole install
    let rows: Vec<(String, String, u32)> = (0..33)
        .map(|k| {
            (
                format!("2026-11-14T{:02}:{:02}:00", 8 + k / 12, 5 * (k % 12)),
                format!("dev-{k}"),
                100,
            )
        })
        .collect();
    let before = source_counter(&env).await?;
    let (_, id) = upload(&env, "lucia", "many.csv", &clarity_rows(&rows)).await?;
    let (state, _, err) = job(&env, id).await?;
    assert_eq!(state, "dead", "{err:?}");
    assert!(
        err.as_deref().unwrap_or_default().contains("more than 32 sensors"),
        "{err:?}"
    );
    // nothing was written, and the counter of source ids did not move
    assert_eq!(source_counter(&env).await?, before);
    assert_eq!(
        env.count(
            "select count(*) from wearable_source where profile_id = $1",
            env.fam.profile("lucia")
        )
        .await?,
        0
    );
    // 32 sensors is still a file
    let (_, id) = upload(&env, "lucia", "ok.csv", &clarity_rows(&rows[..32])).await?;
    assert_eq!(job(&env, id).await?.0, "done");
    Ok(())
}

#[test]
fn device_names_and_serials_from_the_file_are_cut() -> R {
    let long = "G".repeat(300);
    let mut text = String::from_utf8(fixture("libreview-en-mgdl.csv")?)?;
    text = text
        .replace("FreeStyle Libre 3", &long)
        .replace("0B5A1C2D-7E3F-4A90-8C61-5D2E0F1A9C3B", &long);
    let p = cgm_csv::parse(text.as_bytes())?;
    assert_eq!(p.devices.len(), 1);
    assert_eq!(p.devices[0].name.chars().count(), 80);
    assert_eq!(p.devices[0].external_id.as_deref().map(|s| s.chars().count()), Some(80));
    Ok(())
}

#[tokio::test]
async fn a_file_cut_inside_its_last_reading_counts_it_as_cut() -> R {
    let env = Env::new().await?;
    // LibreView cut inside the value 131 of its fifth reading: the last record is "...,0,13"
    let text = String::from_utf8(fixture("libreview-en-mgdl.csv")?)?;
    let at = text.find(",0,131,").map(|i| i + ",0,13".len()).unwrap_or_default();
    let (_, id) = upload(&env, "lucia", "libre-cut.csv", &text.as_bytes()[..at]).await?;
    let (state, result, err) = job(&env, id).await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["accepted"], json!(4), "{result}");
    assert_eq!(result["reasons"], json!({"cut_row": 1}), "{result}");
    assert!(!values(&readings(&env, "lucia").await?).contains(&13.0));
    // Clarity cut inside its last reading, 133
    let env = Env::new().await?;
    let text = String::from_utf8(fixture("clarity-en-mgdl.csv")?)?;
    let at = text
        .find("Android G7,133,")
        .map(|i| i + "Android G7,13".len())
        .unwrap_or_default();
    let (_, id) = upload(&env, "lucia", "clarity-cut.csv", &text.as_bytes()[..at]).await?;
    let (_, result, _) = job(&env, id).await?;
    assert_eq!(result["accepted"], json!(7), "{result}");
    assert_eq!(result["reasons"]["cut_row"], json!(1), "{result}");
    assert!(!values(&readings(&env, "lucia").await?).contains(&13.0));
    // a whole last row without a final line break is a reading, and so is one cut after its value
    let env = Env::new().await?;
    let mut bytes = clarity_rows(&[("2026-11-14T08:00:00".into(), "iOS G7".into(), 101)]);
    bytes.pop();
    let (_, id) = upload(&env, "lucia", "no-newline.csv", &bytes).await?;
    let (_, result, _) = job(&env, id).await?;
    assert_eq!(
        (result["accepted"].clone(), result["reasons"].clone()),
        (json!(1), json!({})),
        "{result}"
    );
    let env = Env::new().await?;
    let at = text
        .find("Android G7,133,")
        .map(|i| i + "Android G7,133,,".len())
        .unwrap_or_default();
    let (_, id) = upload(&env, "lucia", "cut-after.csv", &text.as_bytes()[..at]).await?;
    let (_, result, _) = job(&env, id).await?;
    assert_eq!(result["accepted"], json!(8), "{result}");
    Ok(())
}

/// Enqueue the upload job of a stored blob as the upload route does, with the browser's zone.
async fn upload_in_zone(env: &Env, slug: &str, name: &str, bytes: &[u8], zone: &str) -> anyhow::Result<Value> {
    let sha = env.blobs.put_bytes(BlobOwner::Profile(env.fam.profile(slug)), bytes)?;
    let mut c = env.db.pool.acquire().await?;
    let id = jobs::enqueue(
        &mut c,
        cgm_csv::JOB,
        &json!({
            "profile_id": env.fam.profile(slug).to_string(),
            "household_id": env.fam.household_id.to_string(),
            "blob_sha256": sha,
            "filename": name,
            "timezone": zone,
        }),
        jobs::EnqueueOptions::default(),
    )
    .await?;
    drop(c);
    let (state, result, err) = job(env, id).await?;
    assert_eq!(state, "done", "{err:?}");
    Ok(result)
}

#[tokio::test]
async fn the_browser_zone_reads_the_times_when_the_server_has_none() -> R {
    let env = Env::new().await?;
    let result = upload_in_zone(
        &env,
        "lucia",
        "clarity.csv",
        &fixture("clarity-en-mgdl.csv")?,
        "Europe/Madrid",
    )
    .await?;
    assert_eq!(result["cgm_csv"]["timezone"], json!("Europe/Madrid"), "{result}");
    let r = readings(&env, "lucia").await?;
    // 20:00:17 in Madrid in November is 19:00:17 UTC (CET, +60)
    assert_eq!(r[0], (utc(2026, 11, 14, 19, 0, 17), 120.0, Some(60)));
    let flags: Value = sqlx::query_scalar("select flags->'cgm_csv'->'timezone' from document where profile_id = $1")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(flags, json!("Europe/Madrid"));
    // a zone Postgres does not know is no zone: the job reads the times as UTC
    let env = Env::new().await?;
    let result = upload_in_zone(
        &env,
        "lucia",
        "clarity.csv",
        &fixture("clarity-en-mgdl.csv")?,
        "Mars/Olympus",
    )
    .await?;
    assert_eq!(result["cgm_csv"]["timezone"], Value::Null, "{result}");
    assert_eq!(readings(&env, "lucia").await?[0].0, utc(2026, 11, 14, 20, 0, 17));
    Ok(())
}

#[tokio::test]
async fn a_browser_zone_with_an_old_name_reads_the_times_in_its_current_zone() -> R {
    let env = Env::new().await?;
    // Chromium reports India as Asia/Calcutta, a name the Debian Postgres image does not ship
    let result = upload_in_zone(
        &env,
        "lucia",
        "clarity.csv",
        &fixture("clarity-en-mgdl.csv")?,
        "Asia/Calcutta",
    )
    .await?;
    assert_eq!(result["cgm_csv"]["timezone"], json!("Asia/Kolkata"), "{result}");
    // 20:00:17 in India is 14:30:17 UTC (IST, +330)
    let r = readings(&env, "lucia").await?;
    assert_eq!(r[0], (utc(2026, 11, 14, 14, 30, 17), 120.0, Some(330)));
    Ok(())
}

/// Each reading of the profile's CGM exports with the file that owns it.
async fn owned(env: &Env, slug: &str) -> anyhow::Result<Vec<(String, DateTime<Utc>, f32)>> {
    Ok(sqlx::query_as(
        "select d.filename, s.t, s.v from wearable_sample s join document d on d.id = s.document_id \
         where s.profile_id = $1 and s.metric = 'glucose_cgm' order by d.filename, s.t",
    )
    .bind(env.fam.profile(slug))
    .fetch_all(&env.db.pool)
    .await?)
}

#[tokio::test]
async fn the_same_file_again_in_another_zone_replaces_its_readings() -> R {
    let env = Env::new().await?;
    let bytes = fixture("clarity-en-mgdl.csv")?;
    import_in_zone(&env, "lucia", &bytes, None).await?;
    assert_eq!(readings(&env, "lucia").await?[0].0, utc(2026, 11, 14, 20, 0, 17));
    // the browser now says Madrid: the file is read again in Madrid and keeps one copy of each reading
    let result = import_in_zone(&env, "lucia", &bytes, Some("Europe/Madrid")).await?;
    assert_eq!(result["accepted"], json!(8), "{result}");
    let r = readings(&env, "lucia").await?;
    assert_eq!(r.len(), 8, "{r:?}");
    assert_eq!(r[0], (utc(2026, 11, 14, 19, 0, 17), 120.0, Some(60)));
    assert!(r.iter().all(|x| x.2 == Some(60)), "{r:?}");
    let flags: Value = sqlx::query_scalar("select flags from document where profile_id = $1")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(flags["cgm_csv"]["timezone"], json!("Europe/Madrid"), "{flags}");
    assert_eq!(flags["cgm_csv"]["replaced"], json!(8), "{flags}");
    assert_eq!(flags["import"]["accepted"], json!(8), "{flags}");
    // the same zone again changes nothing
    let result = import_in_zone(&env, "lucia", &bytes, Some("Europe/Madrid")).await?;
    assert_eq!(result["reasons"]["duplicate"], json!(8), "{result}");
    assert_eq!(readings(&env, "lucia").await?.len(), 8);
    Ok(())
}

#[tokio::test]
async fn a_server_zone_with_an_old_name_reads_the_times_in_its_current_zone() -> R {
    let env = Env::new().await?;
    // OWNHEALTH_TIMEZONE=Asia/Calcutta (the job passes the variable in): it reads as Asia/Kolkata, and
    // it wins over the browser's zone
    let mut c = env.db.pool.acquire().await?;
    let (server, browser) = cgm_csv::known_zones(&mut c, Some("Asia/Calcutta"), Some("Europe/Kiev")).await?;
    drop(c);
    assert_eq!(
        (server.as_deref(), browser.as_deref()),
        (Some("Asia/Kolkata"), Some("Europe/Kyiv"))
    );
    let zone = cgm_csv::zone_choice(server.as_deref(), browser.as_deref());
    let bytes = fixture("clarity-en-mgdl.csv")?;
    import_in_zone(&env, "lucia", &bytes, zone).await?;
    // 20:00:17 in India is 14:30:17 UTC (IST, +330)
    assert_eq!(
        readings(&env, "lucia").await?[0],
        (utc(2026, 11, 14, 14, 30, 17), 120.0, Some(330))
    );
    Ok(())
}

#[tokio::test]
async fn a_zone_postgres_knows_keeps_its_name_and_its_other_names_replace_nothing() -> R {
    let env = Env::new().await?;
    // a Postgres that ships its own zone files knows an old name and its current name both, and the
    // zones the tz database merged into another (Europe/Oslo into Europe/Berlin)
    let known: i64 = sqlx::query_scalar(
        "select count(*) from pg_timezone_names where name in ('Europe/Belfast', 'Europe/London', 'Europe/Oslo', 'UTC')",
    )
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!(
        known, 4,
        "this Postgres lacks one of the names: the two-name case is no longer covered"
    );
    // the name the browser or the server sent is the name the person reads back, when Postgres knows it
    let mut c = env.db.pool.acquire().await?;
    for zone in [
        "Europe/Belfast",
        "Europe/Oslo",
        "Atlantic/Reykjavik",
        "Europe/Amsterdam",
        "UTC",
    ] {
        assert_eq!(
            cgm_csv::known_zone(&mut c, Some(zone)).await?.as_deref(),
            Some(zone),
            "{zone}"
        );
    }
    drop(c);
    let bytes = fixture("clarity-en-mgdl.csv")?;
    let result = upload_in_zone(&env, "lucia", "clarity.csv", &bytes, "Europe/Oslo").await?;
    assert_eq!(result["cgm_csv"]["timezone"], json!("Europe/Oslo"), "{result}");
    let flags: Value = sqlx::query_scalar("select flags->'cgm_csv'->'timezone' from document where profile_id = $1")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(flags, json!("Europe/Oslo"));
    // 20:00:17 in Oslo in November is 19:00:17 UTC (CET, +60)
    assert_eq!(
        readings(&env, "lucia").await?[0],
        (utc(2026, 11, 14, 19, 0, 17), 120.0, Some(60))
    );
    // the same file from a browser in Berlin: one zone by another name, so nothing is replaced
    let result = upload_in_zone(&env, "lucia", "clarity.csv", &bytes, "Europe/Berlin").await?;
    assert_eq!(result["cgm_csv"].get("replaced"), None, "{result}");
    assert_eq!(result["reasons"]["duplicate"], json!(8), "{result}");
    // and the result names the zone the file keeps, as the document screen beside it does
    assert_eq!(result["cgm_csv"]["timezone"], json!("Europe/Oslo"), "{result}");
    // an export saved under an old name Postgres no longer ships (an install whose Postgres knew it),
    // uploaded again under the current one: the same zone, nothing is replaced
    let result = upload_in_zone(&env, "sam", "clarity.csv", &bytes, "Asia/Kolkata").await?;
    assert_eq!(result["cgm_csv"]["timezone"], json!("Asia/Kolkata"), "{result}");
    sqlx::query(
        "update document set flags = jsonb_set(flags, '{cgm_csv,timezone}', '\"Asia/Calcutta\"') where profile_id = $1",
    )
    .bind(env.fam.profile("sam"))
    .execute(&env.db.pool)
    .await?;
    let result = upload_in_zone(&env, "sam", "clarity.csv", &bytes, "Asia/Kolkata").await?;
    assert_eq!(result["cgm_csv"].get("replaced"), None, "{result}");
    assert_eq!(result["cgm_csv"]["timezone"], json!("Asia/Calcutta"), "{result}");
    assert_eq!(result["reasons"]["duplicate"], json!(8), "{result}");
    assert_eq!(readings(&env, "sam").await?.len(), 8);
    // the other way round: read first under the current name, then uploaded under a name linked to it
    let result = upload_in_zone(&env, "alex", "clarity.csv", &bytes, "Europe/Berlin").await?;
    assert_eq!(result["cgm_csv"]["timezone"], json!("Europe/Berlin"), "{result}");
    let result = upload_in_zone(&env, "alex", "clarity.csv", &bytes, "Europe/Oslo").await?;
    assert_eq!(result["cgm_csv"].get("replaced"), None, "{result}");
    assert_eq!(result["cgm_csv"]["timezone"], json!("Europe/Berlin"), "{result}");
    assert_eq!(result["reasons"]["duplicate"], json!(8), "{result}");
    assert_eq!(readings(&env, "alex").await?.len(), 8);
    Ok(())
}

#[tokio::test]
async fn a_stored_zone_postgres_no_longer_knows_is_read_in_its_current_name() -> R {
    let env = Env::new().await?;
    let bytes = fixture("clarity-en-mgdl.csv")?;
    upload_in_zone(&env, "lucia", "clarity.csv", &bytes, "Asia/Kolkata").await?;
    // an export saved under Asia/Calcutta by an install whose Postgres knew that name; this one does not
    let lacks: bool =
        sqlx::query_scalar("select not exists (select 1 from pg_timezone_names where name = 'Asia/Calcutta')")
            .fetch_one(&env.db.pool)
            .await?;
    assert!(
        lacks,
        "this Postgres knows Asia/Calcutta: the case is no longer covered"
    );
    let doc: Uuid = sqlx::query_scalar(
        "update document set flags = jsonb_set(flags, '{cgm_csv,timezone}', '\"Asia/Calcutta\"') - 'import' \
         where profile_id = $1 returning id",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_one(&env.db.pool)
    .await?;
    // its run again is queued, as after another export's retraction, and its readings are gone
    sqlx::query("delete from wearable_sample where document_id = $1")
        .bind(doc)
        .execute(&env.db.pool)
        .await?;
    let mut c = env.db.pool.acquire().await?;
    let id = jobs::enqueue(
        &mut c,
        cgm_csv::JOB,
        &json!({"document_id": doc.to_string()}),
        jobs::EnqueueOptions::default(),
    )
    .await?;
    drop(c);
    let (state, _, err) = job(&env, id).await?;
    assert_eq!(state, "done", "{err:?}");
    // 20:00:17 in India is 14:30:17 UTC (IST, +330), and the file keeps the name it was read under
    let r = readings(&env, "lucia").await?;
    assert_eq!(r.len(), 8, "{r:?}");
    assert_eq!(r[0], (utc(2026, 11, 14, 14, 30, 17), 120.0, Some(330)));
    let zone: Value = sqlx::query_scalar("select flags->'cgm_csv'->'timezone' from document where id = $1")
        .bind(doc)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(zone, json!("Asia/Calcutta"));
    Ok(())
}

/// Whether a backend of this test's database waits on a lock of the kind `event` (`transactionid`,
/// `advisory`) within 10 seconds.
async fn waits_on(env: &Env, event: &str) -> anyhow::Result<bool> {
    for _ in 0..100 {
        let n: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity where datname = current_database() \
             and wait_event_type = 'Lock' and wait_event = $1",
        )
        .bind(event)
        .fetch_one(&env.db.pool)
        .await?;
        if n > 0 {
            return Ok(true);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(false)
}

/// How many backends wait for the CGM export lock of `profile` (`retract::lock_exports`), read from the
/// lock table itself: a waiter leaves it the moment the lock is handed to it.
async fn waiting_for_exports_of(env: &Env, profile: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from pg_locks l, (select hashtextextended($1, 0) as k) x \
         where l.locktype = 'advisory' and not l.granted and l.objsubid = 1 \
         and l.database = (select oid from pg_database where datname = current_database()) \
         and l.classid::bigint = (x.k >> 32) & 4294967295 and l.objid::bigint = x.k & 4294967295",
    )
    .bind(format!("ownhealth:exports:cgm_csv:{profile}"))
    .fetch_one(&env.db.pool)
    .await?)
}

#[tokio::test]
async fn a_run_again_that_waited_for_a_moved_export_takes_its_new_owners_lock() -> R {
    let env = Env::new().await?;
    let (lucia, alex) = (env.fam.profile("lucia"), env.fam.profile("alex"));
    let b = clarity_at(&every_5("2026-11-14", 11, 12));
    let (_, id) = upload(&env, "lucia", "b.csv", &b).await?;
    assert_eq!(job(&env, id).await?.0, "done");
    let doc: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'b.csv'")
        .bind(lucia)
        .fetch_one(&env.db.pool)
        .await?;
    // the file under Alex's key too, as a move re-keys it
    env.blobs.put_bytes(BlobOwner::Profile(alex), &b)?;
    // its run again is queued, as after another export's retraction, and its readings are gone
    sqlx::query("delete from wearable_sample where document_id = $1")
        .bind(doc)
        .execute(&env.db.pool)
        .await?;
    sqlx::query("update document set flags = flags - 'import' where id = $1")
        .bind(doc)
        .execute(&env.db.pool)
        .await?;
    let mut c = env.db.pool.acquire().await?;
    jobs::enqueue(
        &mut c,
        cgm_csv::JOB,
        &json!({"document_id": doc.to_string()}),
        jobs::EnqueueOptions::default(),
    )
    .await?;
    drop(c);
    // a move of the export holds Lucía's export lock: the run waits for it
    let mut moving = env.db.pool.begin().await?;
    ownhealth_ingest_structured::retract::lock_exports(&mut moving, lucia, "cgm_csv").await?;
    let ctx = env.ctx.clone();
    let run = tokio::spawn(async move { jobs::drain(&ctx, "core", 5, None).await });
    let mut seen = false;
    for _ in 0..100 {
        if waiting_for_exports_of(&env, lucia).await? > 0 {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::ensure!(seen, "the run again never waited for Lucía's export lock");
    // a retraction of Alex's runs and holds her lock; the move commits the export to Alex
    let mut alexs = env.db.pool.begin().await?;
    ownhealth_ingest_structured::retract::lock_exports(&mut alexs, alex, "cgm_csv").await?;
    sqlx::query("update document set profile_id = $1 where id = $2")
        .bind(alex)
        .bind(doc)
        .execute(&mut *moving)
        .await?;
    moving.commit().await?;
    // the run reads Alex as the owner now: it waits for her lock before it writes
    let mut waited = false;
    for _ in 0..100 {
        if waiting_for_exports_of(&env, alex).await? > 0 {
            waited = true;
            break;
        }
        if run.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let written: i64 = sqlx::query_scalar("select count(*) from wearable_sample where document_id = $1")
        .bind(doc)
        .fetch_one(&env.db.pool)
        .await?;
    alexs.rollback().await?;
    run.await??;
    anyhow::ensure!(
        waited,
        "the run wrote under Alex without her export lock ({written} readings committed while she held it)"
    );
    assert_eq!(written, 0);
    let owners: Vec<(Uuid, i64)> =
        sqlx::query_as("select profile_id, count(*) from wearable_sample where document_id = $1 group by profile_id")
            .bind(doc)
            .fetch_all(&env.db.pool)
            .await?;
    assert_eq!(owners, vec![(alex, 24)]);
    Ok(())
}

#[tokio::test]
async fn a_retraction_while_the_export_is_read_again_leaves_none_of_its_readings() -> R {
    let env = Env::new().await?;
    let b = clarity_at(&every_5("2026-11-14", 11, 12));
    let (_, id) = upload(&env, "lucia", "b.csv", &b).await?;
    assert_eq!(job(&env, id).await?.0, "done");
    let doc: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'b.csv'")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    // its run again is queued, as after another export's retraction, and its readings are gone
    sqlx::query("delete from wearable_sample where document_id = $1")
        .bind(doc)
        .execute(&env.db.pool)
        .await?;
    sqlx::query("update document set flags = flags - 'import' where id = $1")
        .bind(doc)
        .execute(&env.db.pool)
        .await?;
    let mut c = env.db.pool.acquire().await?;
    jobs::enqueue(
        &mut c,
        cgm_csv::JOB,
        &json!({"document_id": doc.to_string()}),
        jobs::EnqueueOptions::default(),
    )
    .await?;
    drop(c);
    // the run is held after it read b.csv as active: another transaction writes, uncommitted, a
    // reading at b.csv's first instant, and the run's insert waits for it
    let source: i16 =
        sqlx::query_scalar("select id from wearable_source where profile_id = $1 and provider = 'dexcom_clarity'")
            .bind(env.fam.profile("lucia"))
            .fetch_one(&env.db.pool)
            .await?;
    let mut hold = env.db.pool.begin().await?;
    sqlx::query(
        "insert into wearable_sample (profile_id, metric, t, v, source_id) values ($1, 'glucose_cgm', $2, 1, $3)",
    )
    .bind(env.fam.profile("lucia"))
    .bind(utc(2026, 11, 14, 11, 0, 0))
    .bind(source)
    .execute(&mut *hold)
    .await?;
    let ctx = env.ctx.clone();
    let run = tokio::spawn(async move { jobs::drain(&ctx, "core", 5, None).await });
    anyhow::ensure!(
        waits_on(&env, "transactionid").await?,
        "the run again never waited on the held reading"
    );
    // the retraction of b.csv as the route runs it: its facts first, then its status
    let pool = env.db.pool.clone();
    let retraction = tokio::spawn(async move {
        let mut tx = pool.begin().await?;
        ownhealth_ingest_structured::retract::retract_document_facts(&mut tx, doc, "test").await?;
        sqlx::query("update document set status = 'retracted', status_reason = 'test' where id = $1")
            .bind(doc)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        anyhow::Ok(())
    });
    // the retraction waits for the run's export lock; only then is the held reading let go
    anyhow::ensure!(
        waits_on(&env, "advisory").await?,
        "the retraction never waited for the running import's export lock"
    );
    hold.rollback().await?;
    run.await??;
    retraction.await??;
    let left: i64 = sqlx::query_scalar("select count(*) from wearable_sample where document_id = $1")
        .bind(doc)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(left, 0, "a retracted export still holds readings");
    Ok(())
}

#[tokio::test]
async fn a_zone_replacement_brings_back_what_an_overlapping_export_holds() -> R {
    let env = Env::new().await?;
    // two exports of one sensor read as UTC: a.csv 10:00 to 11:55, b.csv 11:00 to 12:55; a.csv came
    // first and owns 11:00 to 11:55
    let a = clarity_at(&every_5("2026-11-14", 10, 11));
    let b = clarity_at(&every_5("2026-11-14", 11, 12));
    for (name, bytes) in [("a.csv", &a), ("b.csv", &b)] {
        let (_, id) = upload(&env, "lucia", name, bytes).await?;
        assert_eq!(job(&env, id).await?.0, "done", "{name}");
    }
    let count = |r: &[(String, DateTime<Utc>, f32)], name: &str| r.iter().filter(|x| x.0 == name).count();
    let r = owned(&env, "lucia").await?;
    assert_eq!((count(&r, "a.csv"), count(&r, "b.csv")), (24, 12));
    // a.csv again from a Madrid browser: its readings move one hour earlier, 09:00 to 10:55 UTC, and
    // leave 11:00 to 11:55, which b.csv still brings
    let result = upload_in_zone(&env, "lucia", "a.csv", &a, "Europe/Madrid").await?;
    assert_eq!(result["cgm_csv"]["replaced"], json!(24), "{result}");
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let r = owned(&env, "lucia").await?;
    assert_eq!((count(&r, "a.csv"), count(&r, "b.csv")), (24, 24), "{r:?}");
    // one reading every 5 minutes from 09:00 to 12:55 UTC, none missing and none twice
    let times: Vec<DateTime<Utc>> = readings(&env, "lucia").await?.into_iter().map(|x| x.0).collect();
    let expected: Vec<DateTime<Utc>> = (0..48)
        .map(|k| utc(2026, 11, 14, 9, 0, 0) + chrono::Duration::minutes(5 * k))
        .collect();
    assert_eq!(times, expected);
    // b.csv was read again in its own zone: none, as UTC
    let zone: Value = sqlx::query_scalar(
        "select flags->'cgm_csv'->'timezone' from document where profile_id = $1 and filename = 'b.csv'",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!(zone, Value::Null);
    Ok(())
}

#[tokio::test]
async fn a_zone_replacement_runs_the_other_export_again_after_its_run_still_marked_running() -> R {
    let env = Env::new().await?;
    let a = clarity_at(&every_5("2026-11-14", 10, 11));
    let b = clarity_at(&every_5("2026-11-14", 11, 12));
    for (name, bytes) in [("a.csv", &a), ("b.csv", &b)] {
        let (_, id) = upload(&env, "lucia", name, bytes).await?;
        assert_eq!(job(&env, id).await?.0, "done", "{name}");
    }
    let b_doc: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'b.csv'")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    // a run of b.csv again (after a retraction) has committed, but its job is not marked done yet: it
    // still holds the key a new request for b.csv would take
    let mut c = env.db.pool.acquire().await?;
    let key = format!("{}:reimport:{b_doc}", cgm_csv::JOB);
    jobs::enqueue(
        &mut c,
        cgm_csv::JOB,
        &json!({"document_id": b_doc.to_string()}),
        jobs::EnqueueOptions::dedupe(&key),
    )
    .await?;
    drop(c);
    let kinds = [cgm_csv::JOB.to_string()];
    let held = jobs::claim(&env.db.pool, "core", "test-worker", Some(&kinds[..]))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no job to claim"))?;
    // a.csv again from a Madrid browser leaves 11:00 to 11:55 UTC: one more run of b.csv brings them
    upload_in_zone(&env, "lucia", "a.csv", &a, "Europe/Madrid").await?;
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let r = owned(&env, "lucia").await?;
    let count = |name: &str| r.iter().filter(|x| x.0 == name).count();
    assert_eq!((count("a.csv"), count("b.csv")), (24, 24), "{r:?}");
    let state: String = sqlx::query_scalar("select state from job where id = $1")
        .bind(held.id)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(state, "running");
    Ok(())
}

#[tokio::test]
async fn a_zone_replacement_runs_the_other_export_again_when_its_follow_up_is_still_marked_running() -> R {
    let env = Env::new().await?;
    let a = clarity_at(&every_5("2026-11-14", 10, 11));
    let b = clarity_at(&every_5("2026-11-14", 11, 12));
    for (name, bytes) in [("a.csv", &a), ("b.csv", &b)] {
        let (_, id) = upload(&env, "lucia", name, bytes).await?;
        assert_eq!(job(&env, id).await?.0, "done", "{name}");
    }
    let b_doc: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'b.csv'")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    // two earlier requests for b.csv: its run again and the follow-up queued after it have both
    // committed, and neither job is marked done yet, so both keys are held
    let key = format!("{}:reimport:{b_doc}", cgm_csv::JOB);
    let kinds = [cgm_csv::JOB.to_string()];
    let mut held = Vec::new();
    for _ in 0..2 {
        let key = match held.last() {
            Some(r) => format!("{key}:after:{r}"),
            None => key.clone(),
        };
        let mut c = env.db.pool.acquire().await?;
        jobs::enqueue(
            &mut c,
            cgm_csv::JOB,
            &json!({"document_id": b_doc.to_string()}),
            jobs::EnqueueOptions::dedupe(&key),
        )
        .await?;
        drop(c);
        let claimed = jobs::claim(&env.db.pool, "core", "test-worker", Some(&kinds[..]))
            .await?
            .ok_or_else(|| anyhow::anyhow!("no job to claim"))?;
        held.push(claimed.id);
    }
    // a third request, a.csv again from a Madrid browser, still runs b.csv once more
    upload_in_zone(&env, "lucia", "a.csv", &a, "Europe/Madrid").await?;
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let r = owned(&env, "lucia").await?;
    let count = |name: &str| r.iter().filter(|x| x.0 == name).count();
    assert_eq!((count("a.csv"), count("b.csv")), (24, 24), "{r:?}");
    let states: Vec<String> = sqlx::query_scalar("select state from job where id = any($1) order by id")
        .bind(&held)
        .fetch_all(&env.db.pool)
        .await?;
    assert_eq!(states, ["running", "running"]);
    Ok(())
}

#[tokio::test]
async fn a_retraction_after_a_zone_change_reads_the_others_in_their_own_zone() -> R {
    let env = Env::new().await?;
    // two exports read in Madrid; then the zone of a new run changes (here: none), and one is retracted
    import_in_zone(&env, "lucia", &fixture("clarity-en-mgdl.csv")?, Some("Europe/Madrid")).await?;
    let sha = env.blobs.put_bytes(
        BlobOwner::Profile(env.fam.profile("lucia")),
        &fixture("libreview-en-mgdl.csv")?,
    )?;
    let mut c = env.db.pool.acquire().await?;
    let other = ownhealth_ingest_structured::jobs::document_of_type(
        &mut c,
        &env.blobs,
        &json!({"profile_id": env.fam.profile("lucia").to_string(), "household_id": env.fam.household_id.to_string(),
                "blob_sha256": sha, "filename": "libre.csv"}),
        "text/csv",
        "upload",
        documents::WEARABLES_DOC_TYPE,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    cgm_csv::import_cgm_csv(&mut c, &env.blobs, other, None, Some("Europe/Madrid")).await?;
    let before = owned(&env, "lucia").await?;
    assert_eq!(before.len(), 8 + 13);
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(other)
        .execute(&mut *c)
        .await?;
    ownhealth_ingest_structured::retract::retract_document_facts(&mut c, other, "wrong file").await?;
    drop(c);
    jobs::drain(&env.ctx, "core", 5, None).await?;
    // the Clarity export is read again in Madrid, the zone of its file: nothing moves, nothing doubles
    let after = owned(&env, "lucia").await?;
    let clarity: Vec<_> = before.into_iter().filter(|x| x.0 == "clarity.csv").collect();
    assert_eq!(after, clarity);
    Ok(())
}

/// The English Clarity fixture with two more readings, and 99 instead of 150 at 20:40:17.
fn clarity_longer_with_another_value() -> anyhow::Result<Vec<u8>> {
    let text = String::from_utf8(clarity_with_more(2)?)?;
    Ok(text.replace("Android G7,150,", "Android G7,99,").into_bytes())
}

#[tokio::test]
async fn the_first_file_keeps_a_reading_another_file_holds_with_another_value() -> R {
    let env = Env::new().await?;
    import(&env, "clarity-en-mgdl.csv").await?;
    let (_, id) = upload(&env, "lucia", "longer.csv", &clarity_longer_with_another_value()?).await?;
    let (_, result, _) = job(&env, id).await?;
    assert_eq!(result["accepted"], json!(2), "{result}");
    let t = utc(2026, 11, 14, 20, 40, 17);
    let r = owned(&env, "lucia").await?;
    assert_eq!(
        r.iter().filter(|x| x.1 == t).collect::<Vec<_>>(),
        vec![&("clarity-en-mgdl.csv".to_string(), t, 150.0)]
    );
    // retracting the longer file leaves every reading of the first one where it was
    let longer: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'longer.csv'")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    let mut c = env.db.pool.acquire().await?;
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(longer)
        .execute(&mut *c)
        .await?;
    ownhealth_ingest_structured::retract::retract_document_facts(&mut c, longer, "wrong file").await?;
    drop(c);
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let first: Vec<_> = r.into_iter().filter(|x| x.0 == "clarity-en-mgdl.csv").collect();
    assert_eq!(first.len(), 8);
    assert_eq!(owned(&env, "lucia").await?, first);
    Ok(())
}

#[tokio::test]
async fn retracting_a_cgm_export_queues_no_other_csv_document() -> R {
    use ownhealth_ingest_structured::apple_health::synthetic::{ExportSpec, write_export_to};
    let env = Env::new().await?;
    // an Apple Health export whose ECGs become text/csv documents of the profile
    let mut zip = std::io::Cursor::new(Vec::new());
    write_export_to(
        &mut zip,
        &ExportSpec {
            days: 2,
            end: chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default(),
            birth_date: chrono::NaiveDate::from_ymd_opt(2017, 6, 21).unwrap_or_default(),
            ecg_count: 1,
            ..ExportSpec::default()
        },
    )?;
    let (kind, id) = upload(&env, "lucia", "export.zip", &zip.into_inner()).await?;
    assert_eq!(kind, "apple_health_zip");
    assert_eq!(job(&env, id).await?.0, "done");
    let ecg: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and doc_type = 'ecg'")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    // an ECG carries no import summary today; give it one, as a later CSV importer would, so only the
    // cgm_csv flag tells it apart from a CGM export
    sqlx::query("update document set flags = flags || '{\"import\": {\"accepted\": 0}}' where id = $1")
        .bind(ecg)
        .execute(&env.db.pool)
        .await?;
    import(&env, "clarity-en-mgdl.csv").await?;
    let (_, id) = upload(&env, "lucia", "libre.csv", &fixture("libreview-en-mgdl.csv")?).await?;
    assert_eq!(job(&env, id).await?.0, "done");
    let libre: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'libre.csv'")
        .bind(env.fam.profile("lucia"))
        .fetch_one(&env.db.pool)
        .await?;
    let mut c = env.db.pool.acquire().await?;
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(libre)
        .execute(&mut *c)
        .await?;
    ownhealth_ingest_structured::retract::retract_document_facts(&mut c, libre, "wrong file").await?;
    let queued: Vec<String> = sqlx::query_scalar(
        "select payload->>'document_id' from job where kind = 'import.cgm_csv' and payload ? 'document_id' \
         and state = 'queued'",
    )
    .fetch_all(&mut *c)
    .await?;
    let clarity: Uuid =
        sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'clarity-en-mgdl.csv'")
            .bind(env.fam.profile("lucia"))
            .fetch_one(&mut *c)
            .await?;
    // the other CGM export is read again; the ECG is not a CGM export and is left alone
    assert_eq!(queued, vec![clarity.to_string()]);
    Ok(())
}

/// The same export uploaded again while a move or a retraction of it holds its export lock and has not yet
/// locked its row (`retract::lock_export_of`, then the row): the upload waits for that lock before it
/// touches the document row. Before, it linked the arrival to the row first (a foreign key share lock),
/// then waited for the lock, while the move waited for the row: a deadlock (40P01).
#[tokio::test]
async fn an_upload_again_while_a_move_holds_the_export_lock_waits_before_the_row() -> R {
    let env = Env::new().await?;
    let lucia = env.fam.profile("lucia");
    let b = clarity_at(&every_5("2026-11-14", 11, 12));
    let (_, id) = upload(&env, "lucia", "b.csv", &b).await?;
    assert_eq!(job(&env, id).await?.0, "done");
    let doc: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'b.csv'")
        .bind(lucia)
        .fetch_one(&env.db.pool)
        .await?;
    // a move of the export holds its export lock, as the assign route takes it first
    let mut moving = env.db.pool.begin().await?;
    ownhealth_ingest_structured::retract::lock_export_of(&mut moving, doc).await?;
    // the same file uploaded again: its job waits for Lucía's export lock
    let (_, again) = upload(&env, "lucia", "b.csv", &b).await?;
    let ctx = env.ctx.clone();
    let run = tokio::spawn(async move { jobs::drain(&ctx, "core", 1, None).await });
    let mut seen = false;
    for _ in 0..100 {
        if waiting_for_exports_of(&env, lucia).await? > 0 {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::ensure!(seen, "the upload again never waited for Lucía's export lock");
    // the move locks the row, as lock_owner and assign_document do, and commits
    sqlx::query("select id from document where id = $1 for update")
        .bind(doc)
        .execute(&mut *moving)
        .await?;
    moving.commit().await?;
    run.await??;
    let (state, attempts, err): (String, i32, Option<String>) =
        sqlx::query_as("select state, attempts, last_error from job where id = $1")
            .bind(again)
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!((state.as_str(), attempts, err), ("done", 1, None));
    Ok(())
}

/// A retraction that waited for the export lock while a move committed the export to another profile:
/// the retraction takes the new owner's lock too, so the new owner's import (the move queued it) waits
/// for the retraction and brings nothing back. Before, the retraction held the old owner's lock only and
/// the import wrote the readings of an export the retraction then marked retracted.
#[tokio::test]
async fn a_retraction_that_waited_for_a_move_takes_the_new_owners_lock_too() -> R {
    let env = Env::new().await?;
    let (lucia, alex) = (env.fam.profile("lucia"), env.fam.profile("alex"));
    let b = clarity_at(&every_5("2026-11-14", 11, 12));
    let (_, id) = upload(&env, "lucia", "b.csv", &b).await?;
    assert_eq!(job(&env, id).await?.0, "done");
    let doc: Uuid = sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'b.csv'")
        .bind(lucia)
        .fetch_one(&env.db.pool)
        .await?;
    env.blobs.put_bytes(BlobOwner::Profile(alex), &b)?;
    // a move of the export holds Lucía's export lock (the assign route takes lock_export_of first)
    let mut moving = env.db.pool.begin().await?;
    ownhealth_ingest_structured::retract::lock_exports(&mut moving, lucia, "cgm_csv").await?;
    // a retraction of the same export, as the route runs it; it pauses before its commit
    let (facts_tx, facts_rx) = tokio::sync::oneshot::channel::<()>();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let pool = env.db.pool.clone();
    let retraction = tokio::spawn(async move {
        let mut tx = pool.begin().await?;
        ownhealth_ingest_structured::retract::retract_document_facts(&mut tx, doc, "test").await?;
        sqlx::query("update document set status = 'retracted', status_reason = 'test' where id = $1")
            .bind(doc)
            .execute(&mut *tx)
            .await?;
        let _ = facts_tx.send(());
        let _ = go_rx.await;
        tx.commit().await?;
        anyhow::Ok(())
    });
    let mut seen = false;
    for _ in 0..100 {
        if waiting_for_exports_of(&env, lucia).await? > 0 {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::ensure!(seen, "the retraction never waited for Lucía's lock");
    // the move commits as assign_document does: Lucía's readings leave, the export is Alex's, and its
    // import for Alex is queued
    sqlx::query("delete from wearable_sample where document_id = $1")
        .bind(doc)
        .execute(&mut *moving)
        .await?;
    sqlx::query("update document set profile_id = $1, flags = flags - 'import' where id = $2")
        .bind(alex)
        .bind(doc)
        .execute(&mut *moving)
        .await?;
    jobs::enqueue(
        &mut moving,
        cgm_csv::JOB,
        &json!({"document_id": doc.to_string(), "profile_id": alex.to_string()}),
        jobs::EnqueueOptions::default(),
    )
    .await?;
    moving.commit().await?;
    facts_rx.await?;
    // the retraction holds Alex's lock as well: her import waits for it
    let ctx = env.ctx.clone();
    let run = tokio::spawn(async move { jobs::drain(&ctx, "core", 5, None).await });
    let mut waited = false;
    for _ in 0..100 {
        if waiting_for_exports_of(&env, alex).await? > 0 {
            waited = true;
            break;
        }
        if run.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = go_tx.send(());
    retraction.await??;
    run.await??;
    anyhow::ensure!(
        waited,
        "Alex's import ran beside the retraction, which held Lucía's lock only"
    );
    let (status, held): (String, i64) = sqlx::query_as(
        "select status, (select count(*) from wearable_sample where document_id = d.id) from document d where id = $1",
    )
    .bind(doc)
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!(
        (status.as_str(), held),
        ("retracted", 0),
        "a retracted export holds readings"
    );
    Ok(())
}
