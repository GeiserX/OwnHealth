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
    let sha = put(env, "lucia", bytes)?;
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
    // the same activity as .fit.gz: the same stored copy, so the same document, and nothing new
    let (state, gz, err) = import(&env, &fw::gzip(&run), "run.fit.gz").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(gz["document_id"], first["document_id"]);
    assert_eq!(
        (gz["accepted"].clone(), gz["reasons"].clone()),
        (json!(0), json!({"duplicate": 32}))
    );
    let mime: String = sqlx::query_scalar("select mime from document where id = $1")
        .bind(Uuid::parse_str(gz["document_id"].as_str().unwrap_or_default())?)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(mime, "application/vnd.ant.fit");
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
    // a .fit.gz whose first block does not unpack still goes to the FIT importer, which says it is damaged
    let mut garbled = gz.clone();
    for b in &mut garbled[10..40] {
        *b ^= 0xFF;
    }
    assert_eq!(sniff("garbled-run.fit.gz", &garbled, None), "fit_gz");
    assert_eq!(sniff("notes.txt.gz", &garbled, None), "vcf");
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

/// A FIT document as stored.
struct Stored {
    bytes: Vec<u8>,
    sha: String,
    /// (mime, filename, doc_type)
    kind: (String, Option<String>, Option<String>),
    collected: Option<DateTime<Utc>>,
    flags: Value,
}

async fn stored(env: &Env, doc: &Value) -> anyhow::Result<Stored> {
    let id = Uuid::parse_str(doc.as_str().unwrap_or_default())?;
    let row = sqlx::query(
        "select encode(blob_sha256, 'hex') as sha, mime, filename, doc_type, collected_at, flags from document \
         where id = $1",
    )
    .bind(id)
    .fetch_one(&env.db.pool)
    .await?;
    use sqlx::Row as _;
    let sha: String = row.try_get("sha")?;
    let owner = ownhealth_core::blobs::BlobOwner::Profile(env.fam.profile("lucia"));
    Ok(Stored {
        bytes: env.blobs.get_bytes(owner, &sha)?,
        sha,
        kind: (row.try_get("mime")?, row.try_get("filename")?, row.try_get("doc_type")?),
        collected: row.try_get("collected_at")?,
        flags: row.try_get("flags")?,
    })
}

/// Every field of a FIT file as fitparser reads it (checksums checked), by name.
fn fields(bytes: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for rec in fitparser::from_bytes(bytes)? {
        for f in rec.fields() {
            out.push((f.name().to_string(), f.value().to_string()));
        }
    }
    Ok(out)
}

fn exists(env: &Env, sha: &str) -> bool {
    exists_for(env, "lucia", sha)
}

fn exists_for(env: &Env, who: &str, sha: &str) -> bool {
    env.blobs
        .exists(ownhealth_core::blobs::BlobOwner::Profile(env.fam.profile(who)), sha)
}

/// Store bytes for one profile, as the upload route does.
fn put(env: &Env, who: &str, bytes: &[u8]) -> anyhow::Result<String> {
    Ok(env
        .blobs
        .put_bytes(ownhealth_core::blobs::BlobOwner::Profile(env.fam.profile(who)), bytes)?)
}

#[tokio::test]
async fn the_stored_file_holds_no_position_and_the_upload_is_deleted() -> R {
    let env = Env::new().await?;
    let run = fw::run_with_gps();
    let upload_sha = put(&env, "lucia", &run)?;
    let (state, result, err) = import(&env, &run, "run.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    let Stored {
        bytes,
        sha,
        kind,
        collected,
        flags,
    } = stored(&env, &result["document_id"]).await?;
    assert_eq!(
        kind,
        (
            "application/vnd.ant.fit".into(),
            Some("run.fit".into()),
            Some("wearables".into())
        )
    );
    // the stored copy reads (its checksum holds) with every reading but no position
    let before = fit::read_fit(&run)?;
    let after = fit::read_fit(&bytes)?;
    assert!(before.positions && !after.positions);
    assert!(after.position_fields.is_empty());
    assert_eq!((after.hr.clone(), after.sessions.clone()), (before.hr, before.sessions));
    let names = fields(&bytes)?;
    assert!(!names.is_empty());
    assert!(names.iter().all(|(n, _)| !position_key(n)), "{names:?}");
    assert!(fields(&run)?.iter().any(|(n, _)| position_key(n)));
    // the upload is gone, the copy is the document's file
    assert_ne!(sha, upload_sha);
    assert!(!exists(&env, &upload_sha));
    assert!(exists(&env, &sha));
    // what the file holds and the day of its activity stay on the document
    assert_eq!(collected, Some(at(fw::RUN_START)));
    assert_eq!(
        flags["fit"],
        json!({"activities": 1, "heart_rate_minutes": 30, "distances": 1, "energy": 1,
               "device": fw::WATCH, "gps_left_out": true})
    );
    // importing the stored copy again keeps what the upload said about the route
    let mut c = env.db.pool.acquire().await?;
    let again = jobs::enqueue(
        &mut c,
        fit::JOB,
        &json!({"document_id": result["document_id"]}),
        EnqueueOptions::default(),
    )
    .await?;
    drop(c);
    let (state, again, err) = job(&env, again).await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(again["fit"]["gps_left_out"], json!(true), "{again}");
    Ok(())
}

#[tokio::test]
async fn a_gzipped_upload_is_stored_as_the_plain_file_without_its_route() -> R {
    let env = Env::new().await?;
    let gz = fw::gzip(&fw::run_with_gps());
    let upload_sha = put(&env, "lucia", &gz)?;
    let (state, result, err) = import(&env, &gz, "Morning run.fit.gz").await?;
    assert_eq!(state, "done", "{err:?}");
    let Stored { bytes, kind, .. } = stored(&env, &result["document_id"]).await?;
    assert_eq!(
        kind,
        (
            "application/vnd.ant.fit".into(),
            Some("Morning run.fit".into()),
            Some("wearables".into())
        )
    );
    assert!(bytes.starts_with(&[14]), "not the plain file");
    assert!(!fit::read_fit(&bytes)?.positions);
    assert_eq!(
        bytes,
        fit::strip(
            &fw::run_with_gps(),
            &fit::read_fit(&fw::run_with_gps())?.position_fields
        )?
        .0
    );
    assert!(!exists(&env, &upload_sha));
    Ok(())
}

#[tokio::test]
async fn a_developer_position_field_and_an_nmea_sentence_are_blanked_too() -> R {
    let env = Env::new().await?;
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    // an app's own field 0, named Latitude, in degrees as a float
    w.describe(3, 0, fw::FLOAT64, "Latitude");
    w.define_dev(1, fw::RECORD, &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)], &[(0, 8, 0)]);
    w.define(2, fw::NMEA_SENTENCE, &[(253, 4, fw::UINT32), (1, 40, fw::STRING)]);
    for i in 0..10i64 {
        let t = fw::fit_time(fw::RUN_START + 30 * i);
        w.data(1, &[fw::V::U32(t), fw::V::U8(140), fw::V::F64(fw::LAT0)]);
        w.data(
            2,
            &[fw::V::U32(t), fw::V::Str("$GPGGA,080000,4024.918,N,00341.070,W", 40)],
        );
    }
    let file = w.finish();
    let read = fit::read_fit(&file)?;
    assert!(read.positions, "the checks can fail: the file holds both");
    let raw = fields(&file)?;
    assert!(raw.iter().any(|(n, _)| n == "Latitude"), "{raw:?}");
    assert!(raw.iter().any(|(_, v)| v.contains("4024.918")), "{raw:?}");
    let (state, result, err) = import(&env, &file, "app.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["fit"]["gps_left_out"], json!(true));
    let bytes = stored(&env, &result["document_id"]).await?.bytes;
    let kept = fields(&bytes)?;
    assert!(!kept.iter().any(|(n, _)| n == "Latitude"), "{kept:?}");
    assert!(
        !kept.iter().any(|(_, v)| v.contains("4024") || v.contains("GPGGA")),
        "{kept:?}"
    );
    assert!(kept.iter().any(|(n, v)| n == "heart_rate" && v == "140"), "{kept:?}");
    assert!(!fit::read_fit(&bytes)?.positions);
    Ok(())
}

#[tokio::test]
async fn a_refused_upload_is_deleted_and_bytes_another_import_waits_for_stay() -> R {
    let env = Env::new().await?;
    // a course is positions only: refused, and nothing of it stays
    let course = fw::course();
    let course_sha = put(&env, "lucia", &course)?;
    let (state, _, _) = import(&env, &course, "course.fit").await?;
    assert_eq!(state, "dead");
    assert!(!exists(&env, &course_sha));
    // the same run for two people, both queued: the first import leaves the bytes to the second
    let run = fw::run_with_gps();
    let sha = put(&env, "lucia", &run)?;
    put(&env, "alex", &run)?;
    let mut c = env.db.pool.acquire().await?;
    let mut ids = Vec::new();
    for who in ["lucia", "alex"] {
        let payload = json!({
            "profile_id": env.fam.profile(who).to_string(),
            "household_id": env.fam.household_id.to_string(),
            "blob_sha256": sha,
            "filename": "run.fit",
        });
        ids.push(jobs::enqueue(&mut c, fit::JOB, &payload, EnqueueOptions::default()).await?);
    }
    drop(c);
    jobs::drain(&env.ctx, "core", 5, None).await?;
    for id in ids {
        let (state, _, err) = job(&env, id).await?;
        assert_eq!(state, "done", "{err:?}");
    }
    assert!(!exists(&env, &sha) && !exists_for(&env, "alex", &sha));
    assert_eq!(
        env.count(
            "select count(*) from document where household_id = $1 and doc_type = 'wearables'",
            env.fam.household_id
        )
        .await?,
        2
    );
    Ok(())
}

/// A file of one heart rate reading a minute.
fn minutes(n: i64) -> Vec<u8> {
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    w.define(1, fw::RECORD, &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)]);
    for i in 0..n {
        w.data(1, &[fw::V::U32(fw::fit_time(fw::RUN_START + 60 * i)), fw::V::U8(120)]);
    }
    w.finish()
}

#[tokio::test]
async fn a_file_brings_at_most_7_days_of_heart_rate_and_1000_activities() -> R {
    let env = Env::new().await?;
    let (state, result, err) = import(&env, &minutes(fit::MAX_MINUTES as i64 + 20), "week.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["fit"]["heart_rate_minutes"], json!(fit::MAX_MINUTES), "{result}");
    assert_eq!(result["reasons"], json!({"over_limit": 20}));
    // 1,001 rides in one file: the first 1,000 count
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    w.define(
        2,
        fw::SESSION,
        &[
            (2, 4, fw::UINT32),
            (5, 1, fw::ENUM),
            (7, 4, fw::UINT32),
            (11, 2, fw::UINT16),
        ],
    );
    for i in 0..1_001i64 {
        let start = fw::fit_time(fw::RUN_START - 86_400 * 30 + 600 * i);
        w.data(
            2,
            &[
                fw::V::U32(start),
                fw::V::U8(fw::CYCLING),
                fw::V::U32(300_000),
                fw::V::U16(50),
            ],
        );
    }
    let (state, result, err) = import(&env, &w.finish(), "rides.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["fit"]["activities"], json!(fit::MAX_ACTIVITIES), "{result}");
    assert_eq!(result["fit"]["energy"], json!(1_000), "{result}");
    assert_eq!(result["reasons"]["over_limit"], json!(1), "{result}");
    Ok(())
}

/// A file with heart rate and the given sessions (start, elapsed seconds), each with 300 kcal.
fn sessions(list: &[(i64, f64)]) -> Vec<u8> {
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    w.define(1, fw::RECORD, &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)]);
    w.data(1, &[fw::V::U32(fw::fit_time(fw::RUN_START)), fw::V::U8(130)]);
    // elapsed as a float64, which the protocol allows and a device can fill with anything
    w.define(
        2,
        fw::SESSION,
        &[
            (2, 4, fw::UINT32),
            (5, 1, fw::ENUM),
            (7, 8, fw::FLOAT64),
            (11, 2, fw::UINT16),
        ],
    );
    for (start, secs) in list {
        w.data(
            2,
            &[
                fw::V::U32(u32::try_from(start - fw::FIT_EPOCH).unwrap_or_default()),
                fw::V::U8(fw::RUNNING),
                fw::V::F64(secs * 1000.0),
                fw::V::U16(300),
            ],
        );
    }
    w.finish()
}

#[tokio::test]
async fn an_activity_out_of_time_or_with_an_end_no_clock_holds_is_counted_not_stored() -> R {
    let env = Env::new().await?;
    let now = Utc::now().timestamp();
    for (what, start, secs) in [
        ("before 2000", 820_000_000, 1800.0),
        ("two days ahead", now + 2 * 86_400, 1800.0),
        ("ends 49 days ahead", now - 3600, 49.0 * 86_400.0),
        ("8 days long", fw::RUN_START - 30 * 86_400, 8.0 * 86_400.0),
        ("an end past any date", fw::RUN_START, 1e300),
    ] {
        let (state, result, err) = import(&env, &sessions(&[(start, secs)]), &format!("{what}.fit")).await?;
        assert_eq!(state, "done", "{what}: {err:?}");
        assert_eq!(result["fit"]["energy"], json!(0), "{what}: {result}");
        assert_eq!(result["reasons"]["bad_time"], json!(1), "{what}: {result}");
    }
    assert_eq!(rows(&env, "active_energy").await?, []);
    Ok(())
}

#[tokio::test]
async fn two_activities_with_one_start_keep_the_first() -> R {
    let env = Env::new().await?;
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    w.define(
        2,
        fw::SESSION,
        &[
            (2, 4, fw::UINT32),
            (5, 1, fw::ENUM),
            (7, 4, fw::UINT32),
            (9, 4, fw::UINT32),
            (11, 2, fw::UINT16),
        ],
    );
    for (km, kcal) in [(5u32, 300u16), (6, 400)] {
        w.data(
            2,
            &[
                fw::V::U32(fw::fit_time(fw::RUN_START)),
                fw::V::U8(fw::RUNNING),
                fw::V::U32(1_800_000),
                fw::V::U32(km * 100_000),
                fw::V::U16(kcal),
            ],
        );
    }
    let (state, result, err) = import(&env, &w.finish(), "twice.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["reasons"], json!({"duplicate": 2}), "{result}");
    let end = Some(at(fw::RUN_START + 1800));
    assert_eq!(rows(&env, "distance").await?, [(at(fw::RUN_START), end, 5.0)]);
    assert_eq!(rows(&env, "active_energy").await?, [(at(fw::RUN_START), end, 300.0)]);
    Ok(())
}

#[tokio::test]
async fn a_file_whose_every_value_is_left_out_holds_no_activity() -> R {
    let env = Env::new().await?;
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    w.define(1, fw::RECORD, &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)]);
    // a watch whose clock was never set, and an activity dated 2100
    for i in 0..5i64 {
        w.data(1, &[fw::V::U32(fw::fit_time(820_000_000 + 60 * i)), fw::V::U8(120)]);
    }
    w.define(
        2,
        fw::SESSION,
        &[
            (2, 4, fw::UINT32),
            (5, 1, fw::ENUM),
            (7, 4, fw::UINT32),
            (11, 2, fw::UINT16),
        ],
    );
    w.data(
        2,
        &[
            fw::V::U32(fw::fit_time(4_102_444_800)),
            fw::V::U8(fw::RUNNING),
            fw::V::U32(1_800_000),
            fw::V::U16(250),
        ],
    );
    let (state, _, err) = import(&env, &w.finish(), "future-only.fit").await?;
    assert_eq!(state, "dead");
    assert!(err.unwrap_or_default().contains("the FIT file holds no activity"));
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

/// A gzip stream that repeats one member and fails once read past `left` bytes: it stands for a bomb
/// far larger than memory, and tells whether the unpacking stopped in time.
struct Bomb {
    member: Vec<u8>,
    at: usize,
    left: usize,
}

impl std::io::Read for Bomb {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::other("read past the end of the test"));
        }
        let n = buf.len().min(self.left).min(self.member.len() - self.at);
        buf[..n].copy_from_slice(&self.member[self.at..self.at + n]);
        self.at = (self.at + n) % self.member.len();
        self.left -= n;
        Ok(n)
    }
}

#[test]
fn a_gzip_bomb_stops_unpacking_at_the_limit() {
    // 16 MiB of zeros gzips to about 16 kB; sixteen of them unpack to 256 MiB
    let member = fw::gzip(&vec![0u8; 16 << 20]);
    let total = member.len() * 16;
    let mut bomb = Bomb {
        member,
        at: 0,
        left: total,
    };
    let e = fit::unpack_gzip(&mut bomb)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("larger than 32 MB once unpacked"), "{e}");
    // it stopped after about two members (and what the decoder reads ahead), long before the end
    assert!(bomb.left > total / 2, "read {} of {total}", total - bomb.left);
}

#[tokio::test]
async fn a_compressed_file_over_the_limit_is_too_large_not_damaged() -> R {
    let env = Env::new().await?;
    let mut big = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    big.resize(fit::MAX_BYTES as usize + 1024, 0);
    let (state, _, err) = import(&env, &big, "huge.fit.gz").await?;
    assert_eq!(state, "dead");
    let err = err.unwrap_or_default();
    assert!(err.contains("larger than 32 MB"), "{err}");
    Ok(())
}

/// Developer data index 0 and the description of its field `number`, `name`, which declares itself the
/// profile field `field` of message `mesg` (`field_description` fields 14 and 15; 0xFFFF is no message).
fn describe_native(w: &mut fw::Writer, number: u8, base: u8, name: &str, mesg: u16, field: u8) {
    w.define(3, fw::DEVELOPER_DATA_ID, &[(3, 1, fw::UINT8)]);
    w.data(3, &[fw::V::U8(0)]);
    w.define(
        3,
        fw::FIELD_DESCRIPTION,
        &[
            (0, 1, fw::UINT8),
            (1, 1, fw::UINT8),
            (2, 1, fw::UINT8),
            (3, 16, fw::STRING),
            (14, 2, fw::UINT16),
            (15, 1, fw::UINT8),
        ],
    );
    w.data(
        3,
        &[
            fw::V::U8(0),
            fw::V::U8(number),
            fw::V::U8(base),
            fw::V::Str(name, 16),
            fw::V::U16(mesg),
            fw::V::U8(field),
        ],
    );
}

/// How often `needle` occurs in `hay`.
fn copies(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

const LAP: u16 = 19;

#[tokio::test]
async fn a_developer_field_that_stands_for_a_profile_position_is_blanked_whatever_its_name() -> R {
    let env = Env::new().await?;
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    // neutral names: record.position_lat, a lap's start_position_lat, record field 1 (position_long)
    // named only by its field, record.heart_rate, which is no position, and the text of an NMEA
    // sentence (nmea_sentence.sentence, 177/1), which holds one
    describe_native(&mut w, 0, fw::SINT32, "pos_y", fw::RECORD, 0);
    describe_native(&mut w, 1, fw::SINT32, "start_y", LAP, 3);
    describe_native(&mut w, 2, fw::SINT32, "x2", 0xFFFF, 1);
    describe_native(&mut w, 3, fw::UINT8, "hr2", fw::RECORD, 3);
    describe_native(&mut w, 4, fw::STRING, "txt", 177, 1);
    w.define_dev(
        1,
        fw::RECORD,
        &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)],
        &[(0, 4, 0), (1, 4, 0), (2, 4, 0), (3, 1, 0), (4, 40, 0)],
    );
    let (lat, lon) = (fw::semicircles(fw::LAT0), fw::semicircles(fw::LON0));
    for i in 0..10i64 {
        w.data(
            1,
            &[
                fw::V::U32(fw::fit_time(fw::RUN_START + 30 * i)),
                fw::V::U8(140),
                fw::V::I32(lat),
                fw::V::I32(lat + 1),
                fw::V::I32(lon),
                fw::V::U8(141),
                fw::V::Str("$GPGGA,080000,4024.918,N,00341.070,W", 40),
            ],
        );
    }
    let file = w.finish();
    // the checks can fail: the file holds the route
    let needles = [lat.to_le_bytes(), (lat + 1).to_le_bytes(), lon.to_le_bytes()];
    for n in &needles {
        assert_eq!(copies(&file, n), 10);
    }
    let nmea = b"4024.918";
    assert_eq!(copies(&file, nmea), 10);
    let (state, result, err) = import(&env, &file, "app.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["fit"]["gps_left_out"], json!(true), "{result}");
    let bytes = stored(&env, &result["document_id"]).await?.bytes;
    for n in &needles {
        assert_eq!(copies(&bytes, n), 0);
    }
    assert_eq!(copies(&bytes, nmea), 0, "the NMEA text stays");
    // the rest stays: the heart rate and the developer field that stands for it
    let kept = fields(&bytes)?;
    assert!(kept.iter().any(|(n, v)| n == "heart_rate" && v == "140"), "{kept:?}");
    assert!(kept.iter().any(|(n, v)| n == "hr2" && v == "141"), "{kept:?}");
    Ok(())
}

#[tokio::test]
async fn developer_positions_written_before_their_description_are_blanked_too() -> R {
    let env = Env::new().await?;
    let lat = fw::LAT0.to_le_bytes();
    // values before and after the description, and values before a description that comes last
    for (case, after, start) in [
        ("before and after", true, fw::RUN_START),
        ("before only", false, fw::RUN_START + 3600),
    ] {
        let mut w = fw::Writer::default();
        fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, start);
        w.define(5, fw::DEVELOPER_DATA_ID, &[(3, 1, fw::UINT8)]);
        w.data(5, &[fw::V::U8(0)]);
        let record = |w: &mut fw::Writer, from: i64| {
            w.define_dev(1, fw::RECORD, &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)], &[(0, 8, 0)]);
            for i in from..from + 5 {
                w.data(
                    1,
                    &[
                        fw::V::U32(fw::fit_time(start + 30 * i)),
                        fw::V::U8(140),
                        fw::V::F64(fw::LAT0),
                    ],
                );
            }
        };
        record(&mut w, 0);
        w.describe(3, 0, fw::FLOAT64, "Latitude");
        if after {
            record(&mut w, 5);
        }
        let file = w.finish();
        assert_eq!(copies(&file, &lat), if after { 10 } else { 5 }, "{case}");
        let (state, result, err) = import(&env, &file, &format!("{case}.fit")).await?;
        assert_eq!(state, "done", "{case}: {err:?}");
        assert_eq!(result["fit"]["gps_left_out"], json!(true), "{case}: {result}");
        let bytes = stored(&env, &result["document_id"]).await?.bytes;
        assert_eq!(copies(&bytes, &lat), 0, "{case}");
        assert!(
            fields(&bytes)?.iter().any(|(n, v)| n == "heart_rate" && v == "140"),
            "{case}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn every_profile_position_field_in_the_table_is_one_fitparser_names_a_position() -> R {
    for (mesg, field) in fit::PROFILE_POSITIONS {
        let mut w = fw::Writer::default();
        fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
        w.define(1, mesg, &[(field, 4, fw::SINT32)]);
        w.data(1, &[fw::V::I32(fw::semicircles(fw::LAT0))]);
        let names: Vec<String> = fitparser::from_bytes(&w.finish())?
            .iter()
            .filter(|r| r.kind().as_u16() == mesg)
            .flat_map(|r| r.fields().iter().map(|f| f.name().to_string()))
            .collect();
        assert!(names.iter().any(|n| position_key(n)), "({mesg}, {field}): {names:?}");
    }
    Ok(())
}

#[tokio::test]
async fn an_excluded_ride_does_not_take_the_start_of_a_run() -> R {
    let env = Env::new().await?;
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    w.define(
        2,
        fw::SESSION,
        &[
            (2, 4, fw::UINT32),
            (5, 1, fw::ENUM),
            (7, 4, fw::UINT32),
            (9, 4, fw::UINT32),
            (11, 2, fw::UINT16),
        ],
    );
    // a ride with a distance and no energy (its distance is not walking or running), then a run
    for (sport, kcal) in [(fw::CYCLING, 0xFFFFu16), (fw::RUNNING, 300)] {
        w.data(
            2,
            &[
                fw::V::U32(fw::fit_time(fw::RUN_START)),
                fw::V::U8(sport),
                fw::V::U32(1_800_000),
                fw::V::U32(500_000),
                fw::V::U16(kcal),
            ],
        );
    }
    let (state, result, err) = import(&env, &w.finish(), "ride-and-run.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["reasons"], json!({"other_sport_distance": 1}), "{result}");
    let end = Some(at(fw::RUN_START + 1800));
    assert_eq!(rows(&env, "distance").await?, [(at(fw::RUN_START), end, 5.0)]);
    assert_eq!(rows(&env, "active_energy").await?, [(at(fw::RUN_START), end, 300.0)]);
    Ok(())
}

#[tokio::test]
async fn a_document_of_the_same_bytes_stored_before_becomes_a_wearables_document() -> R {
    let env = Env::new().await?;
    // the stored copy of this run, uploaded first under a photo's name, so read:records opens it
    let run = fw::run_with_gps();
    let kept = fit::strip(&run, &fit::read_fit(&run)?.position_fields)?.0;
    let sha = put(&env, "lucia", &kept)?;
    let photo: Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes) \
         values ($1, $2, decode($3, 'hex'), 'run.heic', 'image/heic', $4) returning id",
    )
    .bind(env.fam.household_id)
    .bind(env.fam.profile("lucia"))
    .bind(&sha)
    .bind(i64::try_from(kept.len())?)
    .fetch_one(&env.db.pool)
    .await?;
    let (state, result, err) = import(&env, &run, "run.fit").await?;
    assert_eq!(state, "done", "{err:?}");
    assert_eq!(result["document_id"], json!(photo), "{result}");
    let doc_type: Option<String> = sqlx::query_scalar("select doc_type from document where id = $1")
        .bind(photo)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(doc_type.as_deref(), Some("wearables"));
    Ok(())
}

/// Mark the import job `id` as running on another worker, or as queued again.
async fn hold(env: &Env, id: Option<i64>, running: bool) -> anyhow::Result<()> {
    sqlx::query(if running {
        "update job set state = 'running', locked_by = 'another worker', \
         locked_until = now() + interval '10 minutes' where id = $1"
    } else {
        "update job set state = 'queued', locked_by = null, locked_until = null where id = $1"
    })
    .bind(id)
    .execute(&env.db.pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn the_last_of_two_imports_of_one_upload_deletes_it() -> R {
    let env = Env::new().await?;
    let run = fw::run_with_gps();
    let sha = put(&env, "lucia", &run)?;
    put(&env, "alex", &run)?;
    let mut c = env.db.pool.acquire().await?;
    let mut ids = Vec::new();
    for who in ["lucia", "alex"] {
        let payload = json!({
            "profile_id": env.fam.profile(who).to_string(),
            "household_id": env.fam.household_id.to_string(),
            "blob_sha256": sha,
            "filename": "run.fit",
        });
        ids.push(jobs::enqueue(&mut c, fit::JOB, &payload, EnqueueOptions::default()).await?);
    }
    drop(c);
    let set = |id: Option<i64>, running: bool| hold(&env, id, running);
    // Alex's import runs on another worker while Lucía's ends: Lucía's leaves the bytes to it
    set(ids[1], true).await?;
    let (state, _, err) = job(&env, ids[0]).await?;
    assert_eq!(state, "done", "{err:?}");
    assert!(
        exists_for(&env, "alex", &sha),
        "the bytes another import reads were deleted"
    );
    // Alex's ends while Lucía's row still reads running (a worker writes done after its handler)
    set(ids[0], true).await?;
    set(ids[1], false).await?;
    let (state, _, err) = job(&env, ids[1]).await?;
    assert_eq!(state, "done", "{err:?}");
    assert!(
        !exists(&env, &sha) && !exists_for(&env, "alex", &sha),
        "the last import left the upload"
    );
    Ok(())
}

#[tokio::test]
async fn the_last_of_two_imports_of_one_upload_by_one_profile_deletes_it() -> R {
    let env = Env::new().await?;
    let run = fw::run_with_gps();
    // Lucía uploads the same bytes twice: one sealed copy, two imports
    let sha = put(&env, "lucia", &run)?;
    let mut c = env.db.pool.acquire().await?;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let payload = json!({
            "profile_id": env.fam.profile("lucia").to_string(),
            "household_id": env.fam.household_id.to_string(),
            "blob_sha256": sha,
            "filename": "run.fit",
        });
        ids.push(jobs::enqueue(&mut c, fit::JOB, &payload, EnqueueOptions::default()).await?);
    }
    drop(c);
    // the second import runs on another worker while the first ends: the first leaves the bytes to it
    hold(&env, ids[1], true).await?;
    let (state, _, err) = job(&env, ids[0]).await?;
    assert_eq!(state, "done", "{err:?}");
    assert!(exists(&env, &sha), "the bytes the second import reads were deleted");
    // the second ends while the first's row still reads running: it let the bytes go, so they go now
    hold(&env, ids[0], true).await?;
    hold(&env, ids[1], false).await?;
    let (state, _, err) = job(&env, ids[1]).await?;
    assert_eq!(state, "done", "{err:?}");
    assert!(!exists(&env, &sha), "the last import left the upload");
    Ok(())
}

#[tokio::test]
async fn a_developer_field_its_own_file_never_describes_is_blanked() -> R {
    let env = Env::new().await?;
    let lat = fw::LAT0.to_le_bytes();
    // five records of developer field (0, 0) holding a latitude, in a file that describes no developer field
    let undescribed = |start: i64| {
        let mut w = fw::Writer::default();
        fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, start);
        w.define(5, fw::DEVELOPER_DATA_ID, &[(3, 1, fw::UINT8)]);
        w.data(5, &[fw::V::U8(0)]);
        w.define_dev(1, fw::RECORD, &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)], &[(0, 8, 0)]);
        for i in 0..5 {
            w.data(
                1,
                &[
                    fw::V::U32(fw::fit_time(start + 30 * i)),
                    fw::V::U8(140),
                    fw::V::F64(fw::LAT0),
                ],
            );
        }
        w.finish()
    };
    // a chain: the first file names field (0, 0) Latitude, the second uses it with no description of
    // its own, so no reader that follows the protocol can name those bytes
    let mut w = fw::Writer::default();
    fw::file_id(&mut w, 4, fw::SERIAL, fw::WATCH, fw::RUN_START);
    w.describe(3, 0, fw::FLOAT64, "Latitude");
    w.define_dev(1, fw::RECORD, &[(253, 4, fw::UINT32), (3, 1, fw::UINT8)], &[(0, 8, 0)]);
    for i in 0..5 {
        w.data(
            1,
            &[
                fw::V::U32(fw::fit_time(fw::RUN_START + 30 * i)),
                fw::V::U8(140),
                fw::V::F64(fw::LAT0),
            ],
        );
    }
    let mut chain = w.finish();
    chain.extend(undescribed(fw::RUN_START + 3600));
    for (case, file) in [("chain", chain), ("alone", undescribed(fw::RUN_START + 7200))] {
        assert_eq!(copies(&file, &lat), if case == "chain" { 10 } else { 5 }, "{case}");
        let (state, result, err) = import(&env, &file, &format!("{case}.fit")).await?;
        assert_eq!(state, "done", "{case}: {err:?}");
        assert_eq!(result["fit"]["gps_left_out"], json!(true), "{case}: {result}");
        let bytes = stored(&env, &result["document_id"]).await?.bytes;
        assert_eq!(copies(&bytes, &lat), 0, "{case}");
        assert!(
            fields(&bytes)?.iter().any(|(n, v)| n == "heart_rate" && v == "140"),
            "{case}"
        );
    }
    Ok(())
}

/// A backup copying the files holds the copy lock from before its snapshot until its copy ends: the
/// snapshot may still show a row that held the uploaded bytes, so the import deletes its upload only
/// after the copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_stays_while_a_backup_copies_the_files_and_goes_after() -> R {
    let env = Env::new().await?;
    let run = fw::run_with_gps();
    let upload_sha = put(&env, "lucia", &run)?;
    let id = upload(&env, &run, "run.fit").await?;
    let mut backup = env.db.pool.acquire().await?;
    sqlx::query("select pg_catalog.pg_advisory_lock(pg_catalog.hashtext('ownhealth.blob_copy'))")
        .execute(&mut *backup)
        .await?;
    let backup_pid: i32 = sqlx::query_scalar("select pg_catalog.pg_backend_pid()")
        .fetch_one(&mut *backup)
        .await?;
    let ctx = env.ctx.clone();
    let worker = tokio::spawn(async move { jobs::drain(&ctx, "core", 5, None).await });
    // only a session that this backup's copy lock blocks counts: the cluster is shared with other
    // test databases, whose waiters must not end the loop
    let waiting = "select count(*) from pg_catalog.pg_stat_activity where $1 = any(pg_catalog.pg_blocking_pids(pid))";
    let started = std::time::Instant::now();
    loop {
        let n: i64 = sqlx::query_scalar(waiting)
            .bind(backup_pid)
            .fetch_one(&env.db.pool)
            .await?;
        if n > 0 {
            break;
        }
        anyhow::ensure!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "the import never waited for the copy"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        exists(&env, &upload_sha),
        "the upload went while a backup was copying the files"
    );
    sqlx::query("select pg_catalog.pg_advisory_unlock(pg_catalog.hashtext('ownhealth.blob_copy'))")
        .execute(&mut *backup)
        .await?;
    drop(backup);
    worker.await??;
    let (state, _, err) = job(&env, id).await?;
    assert_eq!(state, "done", "{err:?}");
    assert!(!exists(&env, &upload_sha), "the upload stayed after the copy");
    Ok(())
}
