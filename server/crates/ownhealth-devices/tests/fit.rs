//! FIT activity files through the real job path: an upload-shaped payload, the `import.fit` handler,
//! samples owned by the new document. Every file is written here by `fit_writer` (the public FIT
//! protocol and profile); the ones the browser checks upload are committed in `fixtures/fit` and must be
//! what the writer makes. Synthetic activities of Lucía Rivera.

mod common;
mod fit_writer;

use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use common::{Env, R};
use fit_writer as fw;
use ownhealth_core::jobs::{self, EnqueueOptions};
use ownhealth_devices::fit;
use serde_json::{Value, json};
use uuid::Uuid;

async fn upload(env: &Env, bytes: &[u8], filename: &str) -> anyhow::Result<Option<i64>> {
    let sha = env.blobs.put_bytes(bytes)?;
    let mut c = env.db.pool.acquire().await?;
    let payload = json!({
        "profile_id": env.fam.profile("lucia").to_string(),
        "household_id": env.fam.household_id.to_string(),
        "blob_sha256": sha,
        "filename": filename,
    });
    Ok(jobs::enqueue(&mut c, fit::JOB, &payload, EnqueueOptions::default()).await?)
}

/// Run the queue; the job's state, result and last error.
async fn job(env: &Env, id: Option<i64>) -> anyhow::Result<(String, Value, Option<String>)> {
    jobs::drain(&env.ctx, "core", 5, None).await?;
    Ok(
        sqlx::query_as("select state, coalesce(result, 'null'::jsonb), last_error from job where id = $1")
            .bind(id)
            .fetch_one(&env.db.pool)
            .await?,
    )
}

async fn import(env: &Env, bytes: &[u8], filename: &str) -> anyhow::Result<(String, Value, Option<String>)> {
    let id = upload(env, bytes, filename).await?;
    job(env, id).await
}

/// (metric, t, t_end, v) of Lucía's samples, in time order.
async fn rows(env: &Env, metric: &str) -> anyhow::Result<Vec<(DateTime<Utc>, Option<DateTime<Utc>>, f32)>> {
    Ok(
        sqlx::query_as("select t, t_end, v from wearable_sample where profile_id = $1 and metric = $2 order by t")
            .bind(env.fam.profile("lucia"))
            .bind(metric)
            .fetch_all(&env.db.pool)
            .await?,
    )
}

async fn samples(env: &Env) -> anyhow::Result<i64> {
    env.count(
        "select count(*) from wearable_sample where profile_id = $1",
        env.fam.profile("lucia"),
    )
    .await
}

fn at(unix: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(unix, 0).single().unwrap_or_default()
}

#[tokio::test]
async fn a_generated_fit_file_gives_per_minute_heart_rate_and_one_distance_and_energy_per_activity() -> R {
    let env = Env::new().await?;
    let (state, result, err) = import(&env, &fw::run_with_gps(), "Morning run.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    // a record every 5 s for 30 minutes: 360 readings, 30 heart rate minutes, each the minute's mean
    let hr = rows(&env, "hr").await?;
    assert_eq!(hr.len(), 30, "{hr:?}");
    for (m, (t, t_end, v)) in hr.iter().enumerate() {
        assert_eq!(*t, at(fw::RUN_START + 60 * m as i64));
        assert_eq!(*t_end, None);
        assert_eq!(*v, 133.0 + m as f32, "minute {m}");
    }
    // one distance and one energy for the run, from its start to its end
    let end = Some(at(fw::RUN_START + 1800));
    assert_eq!(rows(&env, "distance").await?, [(at(fw::RUN_START), end, 5.23)]);
    assert_eq!(rows(&env, "active_energy").await?, [(at(fw::RUN_START), end, 312.0)]);
    assert_eq!(samples(&env).await?, 32);
    // FIT times are UTC: no offset is stored, even though the activity message carries a local time
    assert_eq!(
        env.count(
            "select count(*) from wearable_sample where profile_id = $1 and tz_offset_min is not null",
            env.fam.profile("lucia"),
        )
        .await?,
        0
    );
    // one device, named by the file, its serial number as the external id
    let sources: Vec<(String, Option<String>, Option<String>)> =
        sqlx::query_as("select provider, device, external_id from wearable_source where profile_id = $1")
            .bind(env.fam.profile("lucia"))
            .fetch_all(&env.db.pool)
            .await?;
    assert_eq!(
        sources,
        [("fit".into(), Some(fw::WATCH.into()), Some(fw::SERIAL.to_string()))]
    );
    assert_eq!(result["accepted"], json!(32), "{result}");
    assert_eq!(
        result["fit"],
        json!({"activities": 1, "heart_rate_minutes": 30, "distances": 1, "energy": 1,
               "device": fw::WATCH, "gps_left_out": true})
    );
    // the samples belong to the document, and the rollups were built
    let doc = Uuid::parse_str(result["document_id"].as_str().unwrap_or_default())?;
    let (mime, channel): (String, String) = sqlx::query_as(
        "select d.mime, s.channel from document d join document_source s on s.document_id = d.id where d.id = $1",
    )
    .bind(doc)
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!((mime.as_str(), channel.as_str()), ("application/vnd.ant.fit", "upload"));
    assert_eq!(
        env.count(
            &format!("select count(*) from wearable_sample where profile_id = $1 and document_id = '{doc}'"),
            env.fam.profile("lucia"),
        )
        .await?,
        32
    );
    assert!(
        env.count(
            "select count(*) from wearable_rollup where profile_id = $1 and metric = 'hr'",
            env.fam.profile("lucia")
        )
        .await?
            > 0
    );
    Ok(())
}

/// Every key and number of a JSON value, recursively.
fn walk(v: &Value, keys: &mut Vec<String>, numbers: &mut Vec<f64>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                keys.push(k.clone());
                walk(x, keys, numbers);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, keys, numbers)),
        Value::Number(n) => numbers.extend(n.as_f64()),
        Value::String(s) => numbers.extend(s.parse::<f64>().ok()),
        _ => {}
    }
}

/// A number that is one of the file's positions, in degrees or semicircles.
fn is_a_position(n: f64) -> bool {
    let near = |deg: f64| (n - deg).abs() < 0.05;
    let semi = |deg: f64| (n - f64::from(fw::semicircles(deg))).abs() < 2.0e6;
    near(fw::LAT0) || near(fw::LON0) || semi(fw::LAT0) || semi(fw::LON0)
}

fn position_key(k: &str) -> bool {
    let k = k.to_lowercase();
    k.ends_with("_lat")
        || k.ends_with("_long")
        || k.contains("latitude")
        || k.contains("longitude")
        || k.contains("position")
}

#[tokio::test]
async fn no_stored_row_flag_result_or_review_item_holds_a_position() -> R {
    let env = Env::new().await?;
    let lucia = env.fam.profile("lucia");
    let (state, result, err) = import(&env, &fw::run_with_gps(), "run.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    let doc = Uuid::parse_str(result["document_id"].as_str().unwrap_or_default())?;
    // only the three metrics, and no value is a coordinate
    let stored: Vec<(String, f32)> = sqlx::query_as("select metric, v from wearable_sample where profile_id = $1")
        .bind(lucia)
        .fetch_all(&env.db.pool)
        .await?;
    assert!(!stored.is_empty());
    for (metric, v) in &stored {
        assert!(
            ["hr", "distance", "active_energy"].contains(&metric.as_str()),
            "{metric}"
        );
        assert!(!is_a_position(f64::from(*v)), "{metric} {v}");
    }
    let source_text: Vec<String> = sqlx::query_scalar(
        "select concat_ws(' ', provider, device, external_id) from wearable_source where profile_id = $1",
    )
    .bind(lucia)
    .fetch_all(&env.db.pool)
    .await?;
    for s in &source_text {
        assert!(
            s.split(' ')
                .filter_map(|w| w.parse::<f64>().ok())
                .all(|n| !is_a_position(n)),
            "{s}"
        );
    }
    // the document's flags, the job's result and payload, and any review item of the document
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(doc)
        .fetch_one(&env.db.pool)
        .await?;
    let job_rows: Vec<Value> = sqlx::query_scalar(
        "select jsonb_build_array(payload, coalesce(result, 'null'::jsonb)) from job where kind = 'import.fit'",
    )
    .fetch_all(&env.db.pool)
    .await?;
    let items: Vec<Value> = sqlx::query_scalar("select payload from review_item where document_id = $1")
        .bind(doc)
        .fetch_all(&env.db.pool)
        .await?;
    assert!(items.is_empty(), "{items:?}");
    let (mut keys, mut numbers) = (Vec::new(), Vec::new());
    for v in std::iter::once(&flags).chain(&job_rows) {
        walk(v, &mut keys, &mut numbers);
    }
    assert!(keys.iter().all(|k| !position_key(k)), "{keys:?}");
    assert!(numbers.iter().all(|n| !is_a_position(*n)), "{numbers:?}");
    // the checks can fail: the writer did put positions in the file
    let read = fit::read_fit(&fw::run_with_gps())?;
    assert!(read.positions);
    assert!(is_a_position(f64::from(fw::semicircles(fw::LAT0 + 0.003))));
    assert!(position_key("start_position_lat") && position_key("nec_long"));
    Ok(())
}

#[tokio::test]
async fn the_same_file_again_or_gzipped_adds_nothing() -> R {
    let env = Env::new().await?;
    let run = fw::run_with_gps();
    let (_, first, _) = import(&env, &run, "run.fit").await?;
    assert_eq!(first["accepted"], json!(32), "{first}");
    let (state, again, err) = import(&env, &run, "run.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(again["document_id"], first["document_id"]);
    assert_eq!(again["accepted"], json!(0), "{again}");
    assert_eq!(again["reasons"], json!({"duplicate": 32}));
    // the same activity as .fit.gz: another document, and still nothing new
    let (state, gz, err) = import(&env, &fw::gzip(&run), "run.fit.gz").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_ne!(gz["document_id"], first["document_id"]);
    assert_eq!(
        (gz["accepted"].clone(), gz["reasons"].clone()),
        (json!(0), json!({"duplicate": 32}))
    );
    let mime: String = sqlx::query_scalar("select mime from document where id = $1")
        .bind(Uuid::parse_str(gz["document_id"].as_str().unwrap_or_default())?)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(mime, "application/gzip");
    assert_eq!(samples(&env).await?, 32);
    assert_eq!(
        env.count(
            "select count(*) from wearable_source where profile_id = $1",
            env.fam.profile("lucia")
        )
        .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_ride_brings_heart_rate_and_energy_but_not_walking_distance() -> R {
    let env = Env::new().await?;
    let (state, result, err) = import(&env, &fw::gzip(&fw::ride()), "ride.fit.gz").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(rows(&env, "hr").await?.len(), 20);
    assert_eq!(rows(&env, "distance").await?, []);
    assert_eq!(
        rows(&env, "active_energy").await?,
        [(at(fw::RIDE_START), Some(at(fw::RIDE_START + 1200)), 210.0)]
    );
    assert_eq!(result["reasons"], json!({"other_sport_distance": 1}), "{result}");
    assert_eq!(result["fit"]["gps_left_out"], json!(false));
    Ok(())
}

#[tokio::test]
async fn a_corrupt_or_truncated_fit_file_is_refused_with_a_reason_and_writes_nothing() -> R {
    let env = Env::new().await?;
    let run = fw::run_with_gps();
    let cut = run[..run.len() / 2].to_vec();
    // one bit of the last message (the activity's local time), so only the checksum can tell
    let mut flipped = run.clone();
    let last = flipped.len() - 4;
    flipped[last] ^= 0x01;
    let mut bad_gz = fw::gzip(&run);
    bad_gz.truncate(bad_gz.len() / 2);
    // two files chained, the second without its checksum: its header holds, its end is missing
    let mut chained = run.clone();
    chained.extend_from_slice(&run[..run.len() - 2]);
    for (bytes, name, reason) in [
        (cut, "cut.fit", "not a readable FIT file: it is cut short"),
        (
            flipped,
            "flipped.fit",
            "not a readable FIT file: its checksum does not match",
        ),
        (
            bad_gz,
            "cut.fit.gz",
            "not a readable FIT file: the gzip data is damaged",
        ),
        (chained, "chained.fit", "not a readable FIT file: it is cut short"),
    ] {
        let (state, _, err) = import(&env, &bytes, name).await?;
        assert_eq!(state, "dead", "{name}");
        let err = err.unwrap_or_default();
        assert!(err.starts_with("PermanentJobError: "), "{err}");
        assert!(err.contains(reason), "{name}: {err}");
    }
    assert_eq!(samples(&env).await?, 0);
    assert_eq!(
        env.count(
            "select count(*) from wearable_source where profile_id = $1",
            env.fam.profile("lucia")
        )
        .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn a_fit_file_with_no_activity_data_says_so() -> R {
    let env = Env::new().await?;
    let (state, _, err) = import(&env, &fw::course(), "course.fit").await?;
    assert_eq!(state, "dead");
    assert!(
        err.unwrap_or_default()
            .contains("the FIT file holds no activity: no heart rate, distance or energy")
    );
    assert_eq!(samples(&env).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_gzip_bomb_is_refused_before_it_fills_memory() -> R {
    let env = Env::new().await?;
    // 40 MB of zeros behind a FIT header, gzipped to a few kB
    let mut big = fw::run_with_gps();
    big.resize(40 * 1024 * 1024, 0);
    let bomb = fw::gzip(&big);
    assert!(bomb.len() < 200_000, "{}", bomb.len());
    let (state, _, err) = import(&env, &bomb, "bomb.fit.gz").await?;
    assert_eq!(state, "dead");
    assert!(err.unwrap_or_default().contains("larger than 32 MB once unpacked"));
    assert_eq!(samples(&env).await?, 0);
    Ok(())
}

#[tokio::test]
async fn one_file_makes_at_most_one_wearable_source() -> R {
    let env = Env::new().await?;
    // a chained file of 200 FIT files, each from another device, each with heart rate
    let mut chained = Vec::new();
    for i in 0..200u32 {
        chained.extend(fw::activity(&fw::Activity {
            start: fw::RUN_START + i64::from(i) * 600,
            seconds: 120,
            every: 30,
            sport: fw::SWIMMING,
            gps: false,
            distance_m: 100,
            kcal: 20,
            serial: 9_000 + i,
            product_name: "Watch",
        }));
    }
    let (state, result, err) = import(&env, &chained, "chained.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["fit"]["activities"], json!(200), "{result}");
    let sources: Vec<Option<String>> =
        sqlx::query_scalar("select external_id from wearable_source where profile_id = $1")
            .bind(env.fam.profile("lucia"))
            .fetch_all(&env.db.pool)
            .await?;
    assert_eq!(sources, [Some("9000".to_string())]);
    assert_eq!(rows(&env, "hr").await?.len(), 400);
    Ok(())
}

#[tokio::test]
async fn readings_out_of_range_or_time_are_counted_not_stored() -> R {
    let env = Env::new().await?;
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    w.define(1, fw::RECORD, &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)]);
    // 20/min is not a heart rate; 1995 is a clock that was never set
    for (t, hr) in [(fw::RUN_START, 120u8), (fw::RUN_START + 5, 20), (820_000_000, 120)] {
        w.data(1, &[fw::V::U32(fw::fit_time(t)), fw::V::U8(hr)]);
    }
    let (state, result, err) = import(&env, &w.finish(), "odd.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["reasons"], json!({"bad_time": 1, "bad_value": 1}), "{result}");
    assert_eq!(rows(&env, "hr").await?, [(at(fw::RUN_START), None, 120.0)]);
    Ok(())
}

#[test]
fn fit_and_gzipped_fit_files_are_sniffed_and_a_vcf_stays_a_vcf() {
    use ownhealth_core::documents::{job_kind, mime, sniff};
    let run = fw::run_with_gps();
    assert_eq!(sniff("Morning run.fit", &run[..64], None), "fit");
    // the name does not matter, the header does: a 12-byte header too
    assert_eq!(sniff("export", &run[..64], None), "fit");
    let mut short = run.clone();
    short[0] = 12;
    assert_eq!(sniff("x", &short[..64], None), "fit");
    let gz = fw::gzip(&run);
    assert_eq!(sniff("run.fit.gz", &gz[..gz.len().min(4096)], None), "fit_gz");
    assert_eq!(sniff("activity.gz", &gz[..gz.len().min(4096)], None), "fit_gz");
    let vcf = fw::gzip(b"##fileformat=VCFv4.2\n#CHROM\tPOS\tID\tREF\tALT\n");
    assert_eq!(sniff("sample.vcf.gz", &vcf, None), "vcf");
    assert_eq!(sniff("notes.txt", b"FIT is a file format", None), "unknown");
    assert_eq!(job_kind("fit"), Some(fit::JOB));
    assert_eq!(job_kind("fit_gz"), Some(fit::JOB));
    assert_eq!(mime("fit"), Some("application/vnd.ant.fit"));
    assert_eq!(mime("fit_gz"), Some("application/gzip"));
}

/// `fixtures/fit`, which the browser checks upload, holds what the writer makes: nothing there comes
/// from a device or a person. `OWNHEALTH_WRITE_FIT_FIXTURES=1` writes them again.
#[test]
fn the_committed_fixtures_are_what_the_writer_makes() -> R {
    use std::io::Read as _;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/fit");
    let run = fw::run_with_gps();
    let files: [(&str, Vec<u8>); 4] = [
        ("lucia-run-gps.fit", run.clone()),
        ("lucia-ride.fit.gz", fw::gzip(&fw::ride())),
        ("lucia-run-cut.fit", run[..run.len() / 2].to_vec()),
        ("lucia-course.fit", fw::course()),
    ];
    if std::env::var("OWNHEALTH_WRITE_FIT_FIXTURES").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(&dir)?;
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes)?;
        }
    }
    let unpacked = |b: &[u8]| -> anyhow::Result<Vec<u8>> {
        if !b.starts_with(&[0x1f, 0x8b]) {
            return Ok(b.to_vec());
        }
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(b).read_to_end(&mut out)?;
        Ok(out)
    };
    for (name, bytes) in &files {
        let on_disk = std::fs::read(dir.join(name))?;
        // gzip output may differ between zlib backends: compare what it unpacks to
        assert!(
            unpacked(&on_disk)? == unpacked(bytes)?,
            "{name} is not what fit_writer makes"
        );
    }
    Ok(())
}
