//! Slice 65: the job that turns `wearable_sample` into monthly partitions once it passes
//! OWNHEALTH_PARTITION_ROWS (`ownhealth_ops::partition`, the `ownhealth partition` command), on the
//! demo household with the threshold forced to 1,000 rows.
//!
//! The backup case needs pg_dump and pg_restore 17 on PATH or in OWNHEALTH_PG_BINDIR (see
//! ownhealth-ops/tests/backup.rs). The app-role case makes a login role of its own on the test
//! Postgres and drops it at the end.

use std::io::Cursor;
use std::process::Command;
use std::str::FromStr;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::config::Settings;
use ownhealth_core::demo::{create_rivera_family, seed_demo};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::testing::TestDb;
use ownhealth_devices::store::{self, Conflict, Sample, Written};
use ownhealth_ingest_structured::apple_health::import_apple_health;
use ownhealth_ingest_structured::apple_health::synthetic::{ExportSpec, write_export_to};
use ownhealth_ingest_structured::retract::retract_document_facts;
use ownhealth_ingest_structured::wearables;
use ownhealth_ops::partition::{self, Options, add_months, cutoff_for, month_name, month_start};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
use tempfile::TempDir;
use uuid::Uuid;

type R = anyhow::Result<()>;

const THRESHOLD: u64 = 1_000;
const BIN: &str = env!("CARGO_BIN_EXE_ownhealth");

struct Demo {
    db: TestDb,
    settings: Settings,
    blobs: BlobStore,
    dir: TempDir,
    alex: Uuid,
    household: Uuid,
}

async fn demo() -> anyhow::Result<Demo> {
    ownhealth::register_lanes();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(&dir.path().join("live"))?;
    let blobs = BlobStore::for_settings(&settings)?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    let (alex, household): (Uuid, Uuid) =
        sqlx::query_as("select id, household_id from profile where slug = 'alex' order by created_at desc limit 1")
            .fetch_one(&db.pool)
            .await?;
    Ok(Demo {
        db,
        settings,
        blobs,
        dir,
        alex,
        household,
    })
}

/// The threshold forced to 1,000 rows; short lock waits, so a test that holds the table hears back fast.
fn opts(now: DateTime<Utc>) -> Options {
    Options {
        threshold: THRESHOLD,
        now: Some(now),
        lock_timeout: Duration::from_millis(300),
        attempts: 2,
    }
}

fn state(r: &Value) -> &str {
    r["state"].as_str().unwrap_or_default()
}

async fn relkind(pool: &PgPool) -> anyhow::Result<String> {
    Ok(
        sqlx::query_scalar("select relkind::text from pg_class where oid = 'wearable_sample'::regclass")
            .fetch_one(pool)
            .await?,
    )
}

/// (name, bound) of every partition, by name.
async fn parts(pool: &PgPool) -> anyhow::Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "select c.relname::text, pg_get_expr(c.relpartbound, c.oid) from pg_inherits i \
         join pg_class c on c.oid = i.inhrelid where i.inhparent = 'wearable_sample'::regclass order by 1",
    )
    .fetch_all(pool)
    .await?)
}

/// The row count and a hash of every row, in key order.
async fn fingerprint(pool: &PgPool) -> anyhow::Result<(i64, String)> {
    Ok(sqlx::query_as(
        "select count(*), md5(coalesce(string_agg(w::text, ';' order by w.profile_id, w.metric, w.source_id, w.t), '')) \
         from wearable_sample w",
    )
    .fetch_one(pool)
    .await?)
}

/// The same, with the profile and source named by what they are, so two databases compare.
async fn named_fingerprint(pool: &PgPool) -> anyhow::Result<(i64, String)> {
    Ok(sqlx::query_as(
        "select count(*), md5(coalesce(string_agg(concat_ws('|', p.slug, s.provider, s.device, w.metric, w.t, \
         w.t_end, w.v, w.tz_offset_min, w.document_id is null), ';' order by p.slug, s.provider, s.device, w.metric, w.t, \
         w.v), '')) from wearable_sample w join profile p on p.id = w.profile_id \
         join wearable_source s on s.id = w.source_id",
    )
    .fetch_one(pool)
    .await?)
}

async fn rollups(pool: &PgPool) -> anyhow::Result<(i64, String)> {
    Ok(sqlx::query_as(
        "select count(*), md5(coalesce(string_agg(concat_ws('|', p.slug, s.provider, s.device, r.metric, r.bucket, \
         r.t, r.n, r.v_min, r.v_max, r.v_avg, r.v_sum, r.v_p10, r.v_p50, r.v_p90, r.coverage), ';' \
         order by p.slug, s.provider, s.device, r.metric, r.bucket, r.t, r.n), '')) \
         from wearable_rollup r join profile p on p.id = r.profile_id join wearable_source s on s.id = r.source_id",
    )
    .fetch_one(pool)
    .await?)
}

/// Rebuild every profile's rollups from its samples.
async fn rebuild_all(pool: &PgPool) -> R {
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from profile order by id")
        .fetch_all(pool)
        .await?;
    let mut c = pool.acquire().await?;
    for id in ids {
        wearables::recompute_rollups(&mut c, id, None, None).await?;
    }
    Ok(())
}

async fn where_is(pool: &PgPool, t: DateTime<Utc>) -> anyhow::Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("select tableoid::regclass::text from wearable_sample where t = $1 order by 1")
            .bind(t)
            .fetch_all(pool)
            .await?,
    )
}

fn hr(t: DateTime<Utc>, v: f64, source: i16) -> Sample {
    Sample {
        metric: "hr",
        t,
        t_end: None,
        v,
        tz: None,
        source,
    }
}

async fn watch(c: &mut PgConnection, profile: Uuid) -> anyhow::Result<i16> {
    Ok(store::device_source(c, profile, "apple_health", Some("Partition Watch"), None, None).await?)
}

async fn document(d: &Demo, bytes: &[u8], mime: &str) -> anyhow::Result<Uuid> {
    let sha = d.blobs.put_bytes(BlobOwner::Profile(d.alex), bytes)?;
    let mut c = d.db.pool.acquire().await?;
    let doc = create_document(
        &mut c,
        &NewDocument::upload(d.household, Some(d.alex), &sha, mime, bytes.len() as i64),
    )
    .await?;
    Ok(doc.id)
}

/// Below the threshold the job changes nothing: no index, check or table of its own.
#[tokio::test]
async fn below_the_threshold_the_table_stays_as_the_migrations_made_it() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let before = fingerprint(pool).await?;
    let r = partition::run(
        pool,
        &Options {
            threshold: 10_000_000,
            ..opts(Utc::now())
        },
    )
    .await?;
    assert_eq!(state(&r), "below_threshold", "{r}");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(relkind(pool).await?, "r");
    let indexes: i64 = sqlx::query_scalar("select count(*) from pg_index where indrelid = 'wearable_sample'::regclass")
        .fetch_one(pool)
        .await?;
    assert_eq!(indexes, 1, "only the unique key the migrations made");
    let checks: i64 = sqlx::query_scalar(
        "select count(*) from pg_constraint where conrelid = 'wearable_sample'::regclass and contype = 'c'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(checks, 1, "only wearable_sample_not_a_vendor_score");
    assert_eq!(fingerprint(pool).await?, before);
    Ok(())
}

/// Past the threshold the table becomes monthly range partitions on t. The old table's own file is
/// the historical partition (ATTACH, no row copied), the DEFAULT and the first month after it exist,
/// and the parent carries BRIN on t, a btree on document_id and the unique key. Rows and the rollups
/// rebuilt from them stay the same, and a second run changes nothing.
#[tokio::test]
async fn past_the_threshold_the_old_table_becomes_one_partition_with_no_row_copied() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let before = fingerprint(pool).await?;
    assert!(before.0 > THRESHOLD as i64, "the demo holds {} samples", before.0);
    rebuild_all(pool).await?;
    let rolled = rollups(pool).await?;
    let file: String = sqlx::query_scalar("select pg_relation_filenode('wearable_sample')::text")
        .fetch_one(pool)
        .await?;

    let r = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&r), "converted", "{r}");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(relkind(pool).await?, "p");
    let hist_file: String = sqlx::query_scalar("select pg_relation_filenode('wearable_sample_hist')::text")
        .fetch_one(pool)
        .await?;
    assert_eq!(hist_file, file, "the historical partition is the old table, not a copy");
    assert_eq!(fingerprint(pool).await?, before);
    let elsewhere: i64 =
        sqlx::query_scalar("select count(*) from wearable_sample where tableoid <> 'wearable_sample_hist'::regclass")
            .fetch_one(pool)
            .await?;
    assert_eq!(elsewhere, 0);

    let cutoff = cutoff_for(now);
    let p = parts(pool).await?;
    let names: Vec<&str> = p.iter().map(|(n, _)| n.as_str()).collect();
    for want in [
        "wearable_sample_hist",
        "wearable_sample_default",
        month_name(cutoff).as_str(),
    ] {
        assert!(names.contains(&want), "{want} missing: {p:?}");
    }
    let hist = &p.iter().find(|(n, _)| n == "wearable_sample_hist").expect("hist").1;
    assert!(
        hist.contains("MINVALUE") && hist.contains(&cutoff.format("%Y-%m-%d").to_string()),
        "{hist}"
    );
    let next = add_months(month_start(now), 1);
    assert!(
        next < cutoff || names.contains(&month_name(next).as_str()),
        "next month {next} has no partition: {p:?}"
    );
    let idx: Vec<(String, String)> = sqlx::query_as(
        "select am.amname::text, pg_get_indexdef(i.indexrelid) from pg_index i join pg_class c on c.oid = i.indexrelid \
         join pg_am am on am.oid = c.relam where i.indrelid = 'wearable_sample'::regclass order by 2",
    )
    .fetch_all(pool)
    .await?;
    assert!(
        idx.iter().any(|(am, def)| am == "brin" && def.ends_with("(t)")),
        "{idx:?}"
    );
    assert!(
        idx.iter()
            .any(|(am, def)| am == "btree" && def.ends_with("(document_id)")),
        "{idx:?}"
    );
    assert!(
        idx.iter()
            .any(|(_, def)| def.contains("UNIQUE") && def.ends_with("(profile_id, metric, source_id, t)")),
        "{idx:?}"
    );

    rebuild_all(pool).await?;
    assert_eq!(rollups(pool).await?, rolled, "rollups rebuilt from the partitions");

    let again = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&again), "partitioned", "{again}");
    assert_eq!(again["made"], json!([]), "{again}");
    assert_eq!(parts(pool).await?, p);
    assert_eq!(fingerprint(pool).await?, before);
    Ok(())
}

/// Every write path, the same on a plain table and on a partitioned one.
async fn write_everything(d: &Demo, now: DateTime<Utc>) -> anyhow::Result<Vec<Written>> {
    let mut c = d.db.pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    let base = month_start(now) - chrono::Duration::days(10);
    let first: Vec<Sample> = (0..100)
        .map(|i| hr(base + chrono::Duration::minutes(i), 50.0 + i as f64, src))
        .collect();
    let mut out = vec![store::write(&mut c, d.alex, None, first.clone(), Conflict::Keep).await?];
    out.push(store::write(&mut c, d.alex, None, first.clone(), Conflict::Keep).await?);
    let refreshed: Vec<Sample> = first[..10]
        .iter()
        .map(|s| Sample {
            v: s.v + 1.0,
            ..s.clone()
        })
        .collect();
    out.push(store::write(&mut c, d.alex, None, refreshed, Conflict::Refresh).await?);
    let doc = document(
        d,
        b"a Gadgetbridge export of the partition test",
        "application/x-sqlite3",
    )
    .await?;
    out.push(store::write(&mut c, d.alex, Some(doc), first[..20].to_vec(), Conflict::Adopt).await?);
    // a sample in a month of its own and one a wrong clock dated years ahead
    let ahead = vec![
        hr(cutoff_for(now) + chrono::Duration::hours(30), 70.0, src),
        hr(add_months(month_start(now), 40), 71.0, src),
    ];
    out.push(store::write(&mut c, d.alex, None, ahead, Conflict::Keep).await?);
    drop(c);

    let birth: chrono::NaiveDate = sqlx::query_scalar("select birth_date from profile where id = $1")
        .bind(d.alex)
        .fetch_one(&d.db.pool)
        .await?;
    let mut zip = Cursor::new(Vec::new());
    write_export_to(
        &mut zip,
        &ExportSpec {
            days: 3,
            end: now.date_naive(),
            birth_date: birth,
            ..ExportSpec::default()
        },
    )?;
    let zip = zip.into_inner();
    let doc = document(d, &zip, "application/zip").await?;
    let mut tx = d.db.pool.begin().await?;
    import_apple_health(&mut tx, &d.blobs, doc).await?;
    tx.commit().await?;
    Ok(out)
}

/// A database whose wearable_sample was switched while it held two samples of a family made for the
/// purpose (taken out again), so the demo seed then writes into the partitions.
async fn partitioned_then_demo(now: DateTime<Utc>) -> anyhow::Result<Demo> {
    ownhealth::register_lanes();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(&dir.path().join("live"))?;
    let blobs = BlobStore::for_settings(&settings)?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera before the demo").await?;
    let p = fam.profile("alex");
    let src = watch(&mut c, p).await?;
    let two = vec![
        hr(now - chrono::Duration::days(1), 60.0, src),
        hr(now - chrono::Duration::days(2), 61.0, src),
    ];
    store::write(&mut c, p, None, two, Conflict::Keep).await?;
    drop(c);
    let r = partition::run(
        &db.pool,
        &Options {
            threshold: 1,
            ..opts(now)
        },
    )
    .await?;
    assert_eq!(state(&r), "converted", "{r}");
    for sql in [
        "delete from wearable_rollup where profile_id = $1",
        "delete from wearable_sample where profile_id = $1",
        "delete from wearable_source where profile_id = $1",
    ] {
        sqlx::query(sql).bind(p).execute(&db.pool).await?;
    }
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    let (alex, household): (Uuid, Uuid) =
        sqlx::query_as("select id, household_id from profile where slug = 'alex' order by created_at desc limit 1")
            .fetch_one(&db.pool)
            .await?;
    Ok(Demo {
        db,
        settings,
        blobs,
        dir,
        alex,
        household,
    })
}

/// The demo seed (demo_base and every lane's hook), writes through store.rs (each conflict rule) and
/// an Apple Health import give the same counts and the same rows on a partitioned table as on the
/// plain one.
#[tokio::test]
async fn writes_through_the_store_apple_health_and_the_demo_seed_are_unchanged() -> R {
    let now = Utc::now();
    let plain = demo().await?;
    let parted = partitioned_then_demo(now).await?;
    assert_eq!(relkind(&parted.db.pool).await?, "p");
    assert_eq!(
        named_fingerprint(&parted.db.pool).await?,
        named_fingerprint(&plain.db.pool).await?,
        "the demo seed"
    );
    assert_eq!(rollups(&parted.db.pool).await?, rollups(&plain.db.pool).await?);

    let on_plain = write_everything(&plain, now).await?;
    let on_parted = write_everything(&parted, now).await?;
    assert_eq!(on_parted, on_plain);
    assert_eq!(on_plain[0].new, 100);
    assert_eq!(on_plain[2].refreshed, 10);
    assert_eq!(on_plain[3].refreshed, 20);
    assert_eq!(
        named_fingerprint(&parted.db.pool).await?,
        named_fingerprint(&plain.db.pool).await?
    );
    assert_eq!(rollups(&parted.db.pool).await?, rollups(&plain.db.pool).await?);
    assert_eq!(relkind(&parted.db.pool).await?, "p");
    Ok(())
}

/// A sample past every month lands in DEFAULT. When a later run makes its month (and every month up
/// to the next one), the sample moves into it.
#[tokio::test]
async fn an_insert_outside_every_month_lands_in_default_and_moves_when_its_month_is_made() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    assert_eq!(state(&partition::run(pool, &opts(now)).await?), "converted");
    let mut c = pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    let far = add_months(month_start(now), 14) + chrono::Duration::days(3);
    let w = store::write(&mut c, d.alex, None, vec![hr(far, 61.0, src)], Conflict::Keep).await?;
    drop(c);
    assert_eq!(w.new, 1);
    assert_eq!(where_is(pool, far).await?, ["wearable_sample_default"]);
    let count = fingerprint(pool).await?.0;

    let later = month_start(far) - chrono::Duration::days(1);
    let r = partition::run(pool, &opts(later)).await?;
    assert_eq!(state(&r), "partitioned", "{r}");
    assert_eq!(r["moved_from_default"], 1, "{r}");
    let made = r["made"].as_array().cloned().unwrap_or_default();
    assert!(made.contains(&json!(month_name(month_start(far)))), "{r}");
    assert!(
        made.contains(&json!(month_name(add_months(month_start(far), -1)))),
        "{r}"
    );
    assert_eq!(where_is(pool, far).await?, [month_name(month_start(far))]);
    let in_default: i64 = sqlx::query_scalar("select count(*) from wearable_sample_default")
        .fetch_one(pool)
        .await?;
    assert_eq!(in_default, 0);
    assert_eq!(fingerprint(pool).await?.0, count);
    Ok(())
}

/// A retract by document_id reads the historical partition through the new index, and removes every
/// sample of the document.
#[tokio::test]
async fn a_retract_by_document_id_uses_the_index() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let doc = document(&d, b"a Gadgetbridge export to retract", "application/x-sqlite3").await?;
    let mut c = pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    let base = month_start(now) - chrono::Duration::days(20);
    let samples = (0..200)
        .map(|i| hr(base + chrono::Duration::minutes(i), 60.0, src))
        .collect();
    assert_eq!(
        store::write(&mut c, d.alex, Some(doc), samples, Conflict::Keep)
            .await?
            .new,
        200
    );
    drop(c);
    assert_eq!(state(&partition::run(pool, &opts(now)).await?), "converted");
    sqlx::query("analyze wearable_sample").execute(pool).await?;

    let plan: Value = sqlx::query_scalar(
        "explain (format json) delete from wearable_sample where document_id = $1 \
         returning profile_id, metric, source_id",
    )
    .bind(doc)
    .fetch_one(pool)
    .await?;
    let plan = plan.to_string();
    assert!(plan.contains("wearable_sample_hist_document_id_idx"), "{plan}");

    let mut tx = pool.begin().await?;
    let out = retract_document_facts(&mut tx, doc, "wrong file").await?;
    tx.commit().await?;
    assert_eq!(out["wearable_samples"], 200, "{out}");
    let left: i64 = sqlx::query_scalar("select count(*) from wearable_sample where document_id = $1")
        .bind(doc)
        .fetch_one(pool)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}

/// A run stopped after its preparing steps leaves a table that reads and writes as before; the next
/// run finishes. A concurrent index build cut short (an invalid index) is built again, and a sample a
/// wrong clock dated past the cutoff, kept aside while the check was validated, comes back.
#[tokio::test]
async fn a_run_stopped_between_steps_finishes_on_the_next() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let mut c = pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    let far = add_months(month_start(now), 36) + chrono::Duration::hours(5);
    store::write(&mut c, d.alex, None, vec![hr(far, 58.0, src)], Conflict::Keep).await?;
    drop(c);
    let before = fingerprint(pool).await?;

    let p = partition::prepare(pool, &opts(now)).await?;
    assert_eq!(state(&p), "prepared", "{p}");
    assert_eq!(p["held_aside"], 1, "{p}");
    assert_eq!(relkind(pool).await?, "r");
    let valid: bool = sqlx::query_scalar(
        "select convalidated from pg_constraint where conrelid = 'wearable_sample'::regclass \
         and conname = 'wearable_sample_hist_bound'",
    )
    .fetch_one(pool)
    .await?;
    assert!(valid);
    let aside: i64 = sqlx::query_scalar("select count(*) from wearable_sample_ahead")
        .fetch_one(pool)
        .await?;
    assert_eq!(aside, 1);
    // the app goes on writing today's samples
    let mut c = pool.acquire().await?;
    let today = now - chrono::Duration::minutes(3);
    assert_eq!(
        store::write(&mut c, d.alex, None, vec![hr(today, 66.0, src)], Conflict::Keep)
            .await?
            .new,
        1
    );
    drop(c);
    // a concurrent build the stop cut short
    sqlx::query("update pg_index set indisvalid = false where indexrelid = 'wearable_sample_hist_t_idx'::regclass")
        .execute(pool)
        .await?;

    let r = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&r), "converted", "{r}");
    assert_eq!(r["held_back"], 1, "{r}");
    assert_eq!(relkind(pool).await?, "p");
    assert_eq!(fingerprint(pool).await?.0, before.0 + 1);
    assert_eq!(where_is(pool, far).await?, ["wearable_sample_default"]);
    let invalid: i64 = sqlx::query_scalar(
        "select count(*) from pg_index i join pg_inherits h on h.inhrelid = i.indrelid \
         where h.inhparent = 'wearable_sample'::regclass and not i.indisvalid",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(invalid, 0);
    let gone: Option<String> = sqlx::query_scalar("select to_regclass('wearable_sample_ahead')::text")
        .fetch_one(pool)
        .await?;
    assert_eq!(gone, None);
    Ok(())
}

/// A run that stopped long ago left a cutoff that is now too close (or past): the next run puts the
/// samples back, drops that check and starts again with a cutoff ahead of now.
#[tokio::test]
async fn a_cutoff_that_came_too_close_is_chosen_again() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let before = fingerprint(pool).await?;
    let then = now - chrono::Duration::days(75);
    let p = partition::prepare(pool, &opts(then)).await?;
    assert_eq!(state(&p), "prepared", "{p}");
    assert!(cutoff_for(then) < now);

    let r = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&r), "converted", "{r}");
    assert_eq!(r["cutoff"], json!(cutoff_for(now).to_rfc3339()), "{r}");
    assert_eq!(fingerprint(pool).await?, before);
    Ok(())
}

/// The switch waits for readers for one lock timeout at most, then gives up and says so: it never
/// stands in the lock queue holding every later reader back. Once the reader is gone it goes through.
#[tokio::test]
async fn a_reader_in_the_way_makes_the_switch_give_up_not_queue() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    assert_eq!(state(&partition::prepare(pool, &opts(now)).await?), "prepared");
    let mut reader = PgConnection::connect(&d.db.url).await?;
    let mut tx = reader.begin().await?;
    let n: i64 = sqlx::query_scalar("select count(*) from wearable_sample")
        .fetch_one(&mut *tx)
        .await?;
    let started = Instant::now();
    let r = partition::run(pool, &opts(now)).await?;
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(state(&r), "busy", "{r}");
    assert_eq!(r["ok"], false, "{r}");
    assert_eq!(relkind(pool).await?, "r");
    let still: i64 = sqlx::query_scalar("select count(*) from wearable_sample")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(still, n);
    tx.commit().await?;
    reader.close().await?;
    assert_eq!(state(&partition::run(pool, &opts(now)).await?), "converted");
    Ok(())
}

/// Two runs at once: one converts, the other stands aside.
#[tokio::test]
async fn two_runs_at_once_convert_once() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let before = fingerprint(pool).await?;
    let o = opts(Utc::now());
    let (a, b) = tokio::join!(partition::run(pool, &o), partition::run(pool, &o));
    let mut states = vec![state(&a?).to_string(), state(&b?).to_string()];
    states.sort();
    assert!(
        states == ["busy", "converted"] || states == ["converted", "partitioned"],
        "{states:?}"
    );
    assert_eq!(relkind(pool).await?, "p");
    assert_eq!(fingerprint(pool).await?, before);
    Ok(())
}

async fn export_counts(d: &Demo) -> anyhow::Result<Value> {
    let mut c = d.db.pool.acquire().await?;
    let m = ownhealth_ops::export::export_profile(&mut c, &d.blobs, d.alex, Cursor::new(Vec::new()), "zip").await?;
    Ok(m["counts"].clone())
}

/// The export names wearable_sample once (no partition becomes a file of its own), a backup of the
/// partitioned table restores into an empty database as the same partitions and rows, and a profile
/// delete removes the profile's samples from every partition.
#[tokio::test]
async fn export_backup_restore_and_profile_delete_read_the_partitioned_table() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let exported = export_counts(&d).await?;
    assert_eq!(state(&partition::run(pool, &opts(now)).await?), "converted");
    let after = export_counts(&d).await?;
    assert_eq!(after, exported);
    assert!(
        after
            .as_object()
            .is_some_and(|m| !m.keys().any(|k| k.starts_with("ownhealth/wearable_sample_"))),
        "{after}"
    );

    let archive = ownhealth_ops::backup(&d.settings, &d.dir.path().join("backup").join("staging")).await?;
    let v = ownhealth_ops::verify_restore(&d.settings, &archive).await?;
    assert_eq!(v["ok"], true, "{v}");
    let fresh = TestDb::empty().await?;
    let mut settings = fresh.settings(&d.dir.path().join("fresh"))?;
    settings.secrets_key = d.settings.secrets_key.clone();
    settings.keyring_dir = Some(d.settings.keyring_path());
    let restored = ownhealth_ops::restore_with(&settings, &archive, false).await?;
    assert_eq!(restored["ok"], true, "{restored}");
    assert_eq!(relkind(&fresh.pool).await?, "p");
    assert_eq!(parts(&fresh.pool).await?, parts(pool).await?);
    assert_eq!(fingerprint(&fresh.pool).await?, fingerprint(pool).await?);

    let mut tx = pool.begin().await?;
    ownhealth_auth::household::delete_profile(&mut tx, d.alex).await?;
    tx.commit().await?;
    let left: i64 = sqlx::query_scalar("select count(*) from wearable_sample where profile_id = $1")
        .bind(d.alex)
        .fetch_one(pool)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}

/// The app's own role (deploy/db/app-role.sql: rows, no tables) cannot switch the table and says so;
/// the owner's run does, and the app keeps its rights on the new table.
#[tokio::test]
async fn the_app_role_keeps_its_rights_and_leaves_the_switch_to_the_owner() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let role = TestLogin::new(&d.db, None).await?;
    role.grant_app_rights(&d.db).await?;
    let app = role.connect(&d.db).await?;
    let outcome = async {
        let r = partition::run(&app, &opts(now)).await?;
        assert_eq!(state(&r), "needs_owner", "{r}");
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(relkind(pool).await?, "r");
        assert_eq!(r["cutoff"], Value::Null, "{r}");

        // an owner's run stopped before the switch, with a sample set aside: the app's run names the
        // cutoff that now refuses samples.
        let mut c = pool.acquire().await?;
        let src = watch(&mut c, d.alex).await?;
        let far = cutoff_for(now) + chrono::Duration::days(3);
        store::write(&mut c, d.alex, None, vec![hr(far, 58.0, src)], Conflict::Keep).await?;
        drop(c);
        assert_eq!(partition::prepare(pool, &opts(now)).await?["held_aside"], 1);
        let r = partition::run(&app, &opts(now)).await?;
        assert_eq!(state(&r), "needs_owner", "{r}");
        assert_eq!(r["cutoff"], json!(cutoff_for(now).to_rfc3339()), "{r}");
        let note = r["note"].as_str().unwrap_or_default();
        assert!(note.contains("refused"), "{note}");
        let raised = Options {
            threshold: u64::MAX,
            ..opts(now)
        };
        let r = partition::run(&app, &raised).await?;
        assert_eq!(state(&r), "needs_owner", "{r}");
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(r["cutoff"], json!(cutoff_for(now).to_rfc3339()), "{r}");

        assert_eq!(state(&partition::run(pool, &opts(now)).await?), "converted");
        let mut c = app.acquire().await?;
        let src: i16 = sqlx::query_scalar("select id from wearable_source where profile_id = $1 limit 1")
            .bind(d.alex)
            .fetch_one(&mut *c)
            .await?;
        let today = now - chrono::Duration::minutes(7);
        assert_eq!(
            store::write(&mut c, d.alex, None, vec![hr(today, 64.0, src)], Conflict::Keep)
                .await?
                .new,
            1
        );
        let n: i64 = sqlx::query_scalar("select count(*) from wearable_sample where t = $1")
            .bind(today)
            .fetch_one(&mut *c)
            .await?;
        assert_eq!(n, 1);
        drop(c);
        // making next month's partition needs the owner too
        let later = add_months(month_start(now), 5);
        let r = partition::run(&app, &opts(later)).await?;
        assert_eq!(state(&r), "needs_owner", "{r}");
        anyhow::Ok(())
    }
    .await;
    app.close().await;
    outcome
}

fn cli(d: &Demo, rows: &str) -> anyhow::Result<std::process::Output> {
    Ok(Command::new(BIN)
        .arg("partition")
        .env("OWNHEALTH_DATABASE_URL", &d.db.url)
        .env("OWNHEALTH_DATA_DIR", &d.settings.data_dir)
        .env("OWNHEALTH_PARTITION_ROWS", rows)
        .env_remove("OWNHEALTH_PARTITION_ROWS_FILE")
        .env_remove("OWNHEALTH_DATABASE_URL_FILE")
        .env_remove("OWNHEALTH_DB_PASSWORD")
        .env_remove("OWNHEALTH_DB_PASSWORD_FILE")
        .output()?)
}

/// `ownhealth partition` reads OWNHEALTH_PARTITION_ROWS, prints the report and exits 0 when the table
/// is as it should be; a run below the threshold and a second run change nothing.
#[tokio::test]
async fn the_partition_command_reads_the_threshold_and_prints_the_report() -> R {
    let d = demo().await?;
    let below = cli(&d, "50000000")?;
    assert!(below.status.success(), "{}", String::from_utf8_lossy(&below.stderr));
    let r: Value = serde_json::from_slice(&below.stdout)?;
    assert_eq!(state(&r), "below_threshold", "{r}");
    assert_eq!(r["threshold"], 50_000_000);

    let out = cli(&d, "1000")?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(state(&r), "converted", "{r}");
    let again = cli(&d, "1000")?;
    assert!(again.status.success());
    assert_eq!(state(&serde_json::from_slice(&again.stdout)?), "partitioned");

    let bad = cli(&d, "lots")?;
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("OWNHEALTH_PARTITION_ROWS"),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
    Ok(())
}

/// A sample a wrong clock dated past the cutoff, set aside between the check and the switch, comes
/// back only to a document that still holds it: not after its document moved to another person, and
/// not after its document was retracted. A sample with no document comes back.
#[tokio::test]
async fn a_sample_set_aside_stays_out_when_its_document_moved_or_was_retracted_meanwhile() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let other: Uuid =
        sqlx::query_scalar("select id from profile where household_id = $1 and id <> $2 order by slug limit 1")
            .bind(d.household)
            .bind(d.alex)
            .fetch_one(pool)
            .await?;
    let moved = document(&d, b"a Gadgetbridge export that moves", "application/x-sqlite3").await?;
    let retracted = document(&d, b"a Gadgetbridge export that is retracted", "application/x-sqlite3").await?;
    let far = cutoff_for(now) + chrono::Duration::days(40);
    let (a, b, c) = (
        far,
        far + chrono::Duration::minutes(1),
        far + chrono::Duration::minutes(2),
    );
    let mut conn = pool.acquire().await?;
    let src = watch(&mut conn, d.alex).await?;
    let yesterday = now - chrono::Duration::days(1);
    store::write(
        &mut conn,
        d.alex,
        Some(moved),
        vec![hr(a, 90.0, src), hr(yesterday, 60.0, src)],
        Conflict::Keep,
    )
    .await?;
    store::write(
        &mut conn,
        d.alex,
        Some(retracted),
        vec![hr(b, 91.0, src)],
        Conflict::Keep,
    )
    .await?;
    store::write(&mut conn, d.alex, None, vec![hr(c, 92.0, src)], Conflict::Keep).await?;
    wearables::recompute_rollups(&mut conn, d.alex, None, Some(&[src])).await?;
    drop(conn);

    let role = TestLogin::new(&d.db, None).await?;
    role.grant_app_rights(&d.db).await?;
    let app = role.connect(&d.db).await?;
    let p = partition::prepare(pool, &opts(now)).await?;
    assert_eq!(p["held_aside"], 3, "{p}");

    let mut tx = app.begin().await?;
    let retraction = retract_document_facts(&mut tx, retracted, "wrong file").await?;
    sqlx::query("update document set status = 'retracted', status_reason = 'wrong file' where id = $1")
        .bind(retracted)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut tx = pool.begin().await?;
    ownhealth_ingest_pdf::assign_document(&mut tx, &d.blobs, moved, other, Uuid::new_v4(), None).await?;
    tx.commit().await?;

    let r = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&r), "converted", "{r}");
    assert_eq!(r["held_back"], 1, "{r}");
    for t in [a, b] {
        assert_eq!(where_is(pool, t).await?, Vec::<String>::new(), "{t}");
    }
    assert_eq!(where_is(pool, c).await?, ["wearable_sample_default"]);
    let on_alex: i64 =
        sqlx::query_scalar("select count(*) from wearable_sample where profile_id = $1 and document_id = $2")
            .bind(d.alex)
            .bind(moved)
            .fetch_one(pool)
            .await?;
    assert_eq!(on_alex, 0, "the moved document's samples left the old profile");
    let buckets: Vec<(String, i32, Option<f32>)> = sqlx::query_as(
        "select bucket, n, v_avg from wearable_rollup where profile_id = $1 and metric = 'hr' \
         and source_id = $2 and t >= $3 order by bucket",
    )
    .bind(d.alex)
    .bind(src)
    .bind(month_start(far))
    .fetch_all(pool)
    .await?;
    assert_eq!(buckets, [("day".into(), 1, Some(92.0)), ("hour".into(), 1, Some(92.0))]);
    assert_eq!(retraction["wearable_samples"], 1, "{retraction}");
    Ok(())
}

/// A threshold raised after a run set its check: the next run is below the threshold, takes the check
/// back and puts the samples set aside back, so nothing goes on refusing samples.
#[tokio::test]
async fn a_threshold_raised_after_a_prepare_takes_the_check_back() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let far = cutoff_for(now) + chrono::Duration::days(3);
    let mut c = pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    store::write(&mut c, d.alex, None, vec![hr(far, 58.0, src)], Conflict::Keep).await?;
    drop(c);
    let before = fingerprint(pool).await?;
    let p = partition::prepare(pool, &opts(now)).await?;
    assert_eq!(p["held_aside"], 1, "{p}");

    let r = partition::run(
        pool,
        &Options {
            threshold: 10_000_000,
            ..opts(now)
        },
    )
    .await?;
    assert_eq!(state(&r), "below_threshold", "{r}");
    assert_eq!(r["held_back"], 1, "{r}");
    assert_eq!(relkind(pool).await?, "r");
    let bound: i64 = sqlx::query_scalar(
        "select count(*) from pg_constraint where conrelid = 'wearable_sample'::regclass \
         and conname = 'wearable_sample_hist_bound'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(bound, 0);
    let aside: Option<String> = sqlx::query_scalar("select to_regclass('wearable_sample_ahead')::text")
        .fetch_one(pool)
        .await?;
    assert_eq!(aside, None);
    assert_eq!(fingerprint(pool).await?, before);
    Ok(())
}

/// A run by a member of the owner's role (not the owner itself): the parent, DEFAULT and every month
/// get the owner of the old table, so the owner alone keeps changing them.
#[tokio::test]
async fn a_member_of_the_owners_role_switches_and_the_owner_keeps_every_table() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let owner: String = sqlx::query_scalar(
        "select pg_get_userbyid(relowner)::text from pg_class where oid = 'wearable_sample'::regclass",
    )
    .fetch_one(pool)
    .await?;
    let role = TestLogin::new(&d.db, Some(owner.clone())).await?;
    let name = &role.name;
    sqlx::query(AssertSqlSafe(format!("grant \"{owner}\" to {name}")))
        .execute(pool)
        .await?;
    let member = role.connect(&d.db).await?;
    let outcome = async {
        let r = partition::run(&member, &opts(now)).await?;
        assert_eq!(state(&r), "converted", "{r}");
        let owners: Vec<(String, String)> = sqlx::query_as(
            "select c.relname::text, pg_get_userbyid(c.relowner)::text from pg_class c where c.oid = 'wearable_sample'::regclass \
             or c.oid in (select inhrelid from pg_inherits where inhparent = 'wearable_sample'::regclass) order by 1",
        )
        .fetch_all(pool)
        .await?;
        assert!(owners.len() >= 4, "{owners:?}");
        for (name, o) in &owners {
            assert_eq!(o, &owner, "{name}: {owners:?}");
        }
        anyhow::Ok(())
    }
    .await;
    member.close().await;
    outcome
}

/// A switch kept off by a lock on a table the new parent references (a write on profile left open):
/// the run says busy, and its report keeps the cutoff and the samples it set aside, says that samples
/// dated on or after the cutoff are refused until the next run, and blames no reader.
#[tokio::test]
async fn a_busy_switch_keeps_its_cutoff_and_says_what_waits_for_the_next_run() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let far = cutoff_for(now) + chrono::Duration::days(3);
    let mut c = pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    store::write(&mut c, d.alex, None, vec![hr(far, 58.0, src)], Conflict::Keep).await?;
    drop(c);
    let prepared = partition::prepare(pool, &opts(now)).await?;
    assert_eq!(prepared["held_aside"], 1, "{prepared}");
    let mut writer = PgConnection::connect(&d.db.url).await?;
    let mut tx = writer.begin().await?;
    sqlx::query("update profile set display_name = display_name where id = $1")
        .bind(d.alex)
        .execute(&mut *tx)
        .await?;
    let r = partition::run(pool, &opts(now)).await?;
    tx.rollback().await?;
    writer.close().await?;
    assert_eq!(state(&r), "busy", "{r}");
    assert_eq!(relkind(pool).await?, "r");
    assert_eq!(r["cutoff"], json!(cutoff_for(now).to_rfc3339()), "{r}");
    let note = r["note"].as_str().unwrap_or_default();
    assert!(!note.contains("readers"), "{note}");
    assert!(note.contains("refused") && note.contains("1 samples"), "{note}");

    let again = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&again), "converted", "{again}");
    assert_eq!(again["held_back"], 1, "{again}");
    Ok(())
}

/// Between the check and the switch, an import holding a sample dated on or after the cutoff is
/// refused (store.rs and Apple Health alike). Its job waits an hour and runs again without spending
/// an attempt, so it is never dead before the switch, and its error names no table or constraint.
#[tokio::test]
async fn an_import_the_cutoff_refuses_waits_for_the_switch_without_spending_an_attempt() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    assert_eq!(state(&partition::prepare(pool, &opts(now)).await?), "prepared");
    let far = cutoff_for(now) + chrono::Duration::days(3);

    let mut c = pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    let mut tx = c.begin().await?;
    let refused = store::write(&mut tx, d.alex, None, vec![hr(far, 58.0, src)], Conflict::Keep)
        .await
        .expect_err("a sample past the cutoff is refused");
    tx.rollback().await?;
    drop(c);
    let birth: chrono::NaiveDate = sqlx::query_scalar("select birth_date from profile where id = $1")
        .bind(d.alex)
        .fetch_one(pool)
        .await?;
    let mut zip = Cursor::new(Vec::new());
    write_export_to(
        &mut zip,
        &ExportSpec {
            days: 2,
            end: far.date_naive(),
            birth_date: birth,
            ..ExportSpec::default()
        },
    )?;
    let doc = document(&d, &zip.into_inner(), "application/zip").await?;
    let mut tx = pool.begin().await?;
    let ah = import_apple_health(&mut tx, &d.blobs, doc)
        .await
        .expect_err("an export past the cutoff is refused");
    tx.rollback().await?;

    for e in [refused, ah] {
        let raw = e.to_string();
        let je = ownhealth_core::jobs::JobError::from(e);
        assert!(je.waits, "{raw}");
        assert!(!je.permanent, "{raw}");
        let text = je.text();
        assert!(!text.contains("wearable_sample"), "{text}");
        assert!(text.starts_with("PartitionPendingError: "), "{text}");

        let mut c = pool.acquire().await?;
        let id = ownhealth_core::jobs::enqueue(
            &mut c,
            "s65.wait",
            &json!({"document_id": doc.to_string()}),
            ownhealth_core::jobs::EnqueueOptions {
                max_attempts: 1,
                ..Default::default()
            },
        )
        .await?
        .expect("queued");
        drop(c);
        let job = ownhealth_core::jobs::claim(pool, "core", "w65", Some(&["s65.wait".to_string()]))
            .await?
            .expect("claimed");
        assert_eq!(job.id, id);
        assert_eq!(ownhealth_core::jobs::fail(pool, &job, &je, "w65").await?, "queued");
        let (state, attempts, wait): (String, i32, f64) = sqlx::query_as(
            "select state, attempts, extract(epoch from run_after - now())::float8 from job where id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await?;
        assert_eq!((state.as_str(), attempts), ("queued", 0), "{text}");
        assert!(wait > 3000.0 && wait <= 3600.0, "{wait}");
        sqlx::query("delete from job where id = $1")
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

struct TestLogin {
    name: String,
    password: String,
    database_url: String,
    owner: Option<String>,
}

impl TestLogin {
    async fn new(db: &TestDb, owner: Option<String>) -> anyhow::Result<Self> {
        let name = format!("oh_s65_{}", ownhealth_core::ids::token_hex(6));
        let password = ownhealth_core::ids::token_hex(16);
        sqlx::query(AssertSqlSafe(format!("create role {name} login password '{password}'")))
            .execute(&db.pool)
            .await?;
        Ok(Self {
            name,
            password,
            database_url: db.url.clone(),
            owner,
        })
    }

    async fn grant_app_rights(&self, db: &TestDb) -> R {
        let name = &self.name;
        for sql in [
            format!("grant connect, temporary on database \"{}\" to {name}", db.name),
            format!("grant usage on schema public to {name}"),
            format!("grant select, insert, update, delete on all tables in schema public to {name}"),
            format!("grant usage, select on all sequences in schema public to {name}"),
        ] {
            sqlx::query(AssertSqlSafe(sql)).execute(&db.pool).await?;
        }
        Ok(())
    }

    async fn connect(&self, db: &TestDb) -> anyhow::Result<PgPool> {
        Ok(PgPoolOptions::new()
            .max_connections(3)
            .connect_with(
                PgConnectOptions::from_str(&db.url)?
                    .username(&self.name)
                    .password(&self.password),
            )
            .await?)
    }
}

impl Drop for TestLogin {
    fn drop(&mut self) {
        let name = self.name.clone();
        let database_url = self.database_url.clone();
        let owner = self.owner.clone();
        // A panic can unwind the runtime thread. Use a private runtime so cleanup finishes first.
        let cleanup = std::thread::spawn(move || -> R {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(async move {
                let mut admin = PgConnection::connect(&database_url).await?;
                if let Some(owner) = owner {
                    let owner = owner.replace('"', r#""""#);
                    sqlx::query(AssertSqlSafe(format!(r#"reassign owned by {name} to "{owner}""#)))
                        .execute(&mut admin)
                        .await?;
                }
                sqlx::query(AssertSqlSafe(format!("drop owned by {name}")))
                    .execute(&mut admin)
                    .await?;
                sqlx::query(AssertSqlSafe(format!("drop role {name}")))
                    .execute(&mut admin)
                    .await?;
                Ok(())
            })
        })
        .join();
        if let Ok(Err(error)) = cleanup {
            eprintln!("partition test login cleanup failed: {error}");
        }
    }
}

#[tokio::test]
async fn partition_login_is_removed_after_an_error_or_panic() -> R {
    let db = std::sync::Arc::new(TestDb::empty().await?);
    for panic in [false, true] {
        // Record only roles created by this fixture in this test.
        let (send, recv) = tokio::sync::oneshot::channel();
        let work_db = db.clone();
        let task = tokio::spawn(async move {
            let role = TestLogin::new(&work_db, None).await?;
            let _ = send.send(role.name.clone());
            role.grant_app_rights(&work_db).await?;
            assert!(!panic, "exercise role cleanup during unwind");
            Err::<(), _>(anyhow::anyhow!("exercise role cleanup after an error"))
        });
        let name = recv.await?;
        match task.await {
            Ok(outcome) => assert!(!panic && outcome.is_err()),
            Err(error) => assert!(panic && error.is_panic()),
        }
        let leaked: bool = sqlx::query_scalar("select exists (select 1 from pg_roles where rolname = $1)")
            .bind(&name)
            .fetch_one(&db.pool)
            .await?;
        if leaked {
            for sql in [format!("drop owned by {name}"), format!("drop role {name}")] {
                sqlx::query(AssertSqlSafe(sql)).execute(&db.pool).await?;
            }
        }
        assert!(!leaked, "the test login survived an error or panic");
    }
    Ok(())
}

#[tokio::test]
async fn prepare_leaves_monthly_maintenance_to_run() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    assert_eq!(state(&partition::run(pool, &opts(now)).await?), "converted");
    let later = add_months(month_start(now), 3);
    let before = parts(pool).await?;
    let report = partition::prepare(pool, &opts(later)).await?;
    assert_eq!(state(&report), "partitioned", "{report}");
    assert_eq!(parts(pool).await?, before, "{report}");
    let report = partition::run(pool, &opts(later)).await?;
    assert!(!report["made"].as_array().is_none_or(Vec::is_empty), "{report}");
    Ok(())
}

#[tokio::test]
async fn analyze_during_preparation_counts_staged_rows_toward_the_threshold() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let mut c = pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    store::write(
        &mut c,
        d.alex,
        None,
        vec![hr(cutoff_for(now), 58.0, src)],
        Conflict::Keep,
    )
    .await?;
    drop(c);
    sqlx::query("analyze wearable_sample").execute(pool).await?;
    let (rows, _) = fingerprint(pool).await?;
    let options = Options {
        threshold: u64::try_from(rows - 1)?,
        ..opts(now)
    };
    assert_eq!(partition::prepare(pool, &options).await?["held_aside"], 1);
    sqlx::query("analyze wearable_sample").execute(pool).await?;
    let report = partition::run(pool, &options).await?;
    assert_eq!(state(&report), "converted", "{report}");
    assert_eq!(report["rows"], rows, "{report}");
    Ok(())
}

/// The query deploy/README.md ("Large wearable histories") gives for a `needs_maintenance` report:
/// the files that hold the samples dated on or after the date of the note, largest first. The date is
/// a UTC day, whatever the zone of the session that runs it.
const FUTURE_FILES: &str = "select d.id, d.filename, p.display_name, count(*), min(s.t), max(s.t) from wearable_sample s \
     join profile p on p.id = s.profile_id left join document d on d.id = s.document_id \
     where s.t >= timestamptz 'YYYY-MM-DD 00:00+00' group by d.id, d.filename, p.display_name order by count(*) desc";

/// [`FUTURE_FILES`] as the README prints it, run on `table` (the README names `wearable_sample_ahead`
/// when the report has a `cutoff`) with `day` (YYYY-MM-DD) in place.
async fn future_files(pool: &PgPool, day: &str, table: &str) -> anyhow::Result<Vec<FutureFile>> {
    let readme = include_str!("../../../../deploy/README.md");
    assert!(
        readme.contains(FUTURE_FILES),
        "deploy/README.md gives the query: {FUTURE_FILES}"
    );
    let sql = FUTURE_FILES
        .replace("YYYY-MM-DD", day)
        .replace("from wearable_sample s", &format!("from {table} s"));
    Ok(sqlx::query_as(AssertSqlSafe(sql)).fetch_all(pool).await?)
}

/// A row of [`FUTURE_FILES`]: document, file name, profile, samples, first and last sample time.
type FutureFile = (Option<Uuid>, Option<String>, String, i64, DateTime<Utc>, DateTime<Utc>);

/// More samples past the cutoff than the switch may move: the run changes nothing and says what to
/// do. The way out the README gives works: the query names the file, its retraction removes the
/// samples, and the next run converts the table.
#[tokio::test]
async fn an_oversized_future_group_leaves_the_table_unchanged() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let doc = document(
        &d,
        b"an export from a watch whose clock ran years ahead",
        "application/x-sqlite3",
    )
    .await?;
    let mut c = pool.acquire().await?;
    let src = watch(&mut c, d.alex).await?;
    sqlx::query(
        "insert into wearable_sample (profile_id, metric, t, source_id, v, document_id) \
         select $1, 'hr', $2::timestamptz + n * interval '1 second', $3, 60, $4 \
         from generate_series(1, 50001) n",
    )
    .bind(d.alex)
    .bind(cutoff_for(now))
    .bind(src)
    .bind(doc)
    .execute(&mut *c)
    .await?;
    drop(c);
    let before = fingerprint(pool).await?;
    let mut reader = pool.begin().await?;
    sqlx::query("lock table wearable_sample in access share mode")
        .execute(&mut *reader)
        .await?;
    let report = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&report), "needs_maintenance", "{report}");
    assert_eq!(report["ok"], false, "{report}");
    assert_eq!(report["cutoff"], Value::Null, "no cutoff is left to wait on: {report}");
    let note = report["note"].as_str().unwrap_or_default();
    let since = cutoff_for(now).format("%Y-%m-%d").to_string();
    assert!(note.contains("50001"), "{report}");
    assert!(note.contains(&format!("dated on or after {since}")), "{note}");
    assert!(note.contains("run `ownhealth partition` again"), "{note}");
    assert!(!note.contains("maintenance window"), "a window changes nothing: {note}");
    reader.rollback().await?;
    let added: bool = sqlx::query_scalar(
        "select to_regclass('wearable_sample_ahead') is not null \
         or to_regclass('wearable_sample_hist_t_idx') is not null \
         or exists (select 1 from pg_constraint where conrelid = 'wearable_sample'::regclass \
                    and conname = 'wearable_sample_hist_bound')",
    )
    .fetch_one(pool)
    .await?;
    assert!(!added, "the refused run changed the schema");
    assert_eq!(fingerprint(pool).await?, before);
    let again = partition::run(pool, &opts(now)).await?;
    assert_eq!(
        state(&again),
        "needs_maintenance",
        "nothing resolves it by itself: {again}"
    );

    let files = future_files(pool, &since, "wearable_sample").await?;
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!((files[0].0, files[0].3), (Some(doc), 50001), "{files:?}");
    let mut tx = pool.begin().await?;
    let out = retract_document_facts(&mut tx, doc, "the watch clock was wrong").await?;
    tx.commit().await?;
    assert_eq!(out["wearable_samples"], 50001, "{out}");
    let r = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&r), "converted", "{r}");
    Ok(())
}

/// A Health Auto Export inbox on Alex's profile and the wearable source its pushes write to.
async fn push_inbox(c: &mut PgConnection, d: &Demo) -> anyhow::Result<i16> {
    let inbox: Uuid = sqlx::query_scalar(
        "insert into source (household_id, profile_id, kind, label) \
         values ($1, $2, 'health_auto_export', 'Phone') returning id",
    )
    .bind(d.household)
    .bind(d.alex)
    .fetch_one(&mut *c)
    .await?;
    Ok(store::device_source(
        c,
        d.alex,
        "health_auto_export",
        None,
        Some(&inbox.to_string()),
        Some(inbox),
    )
    .await?)
}

/// A pool whose sessions run in a zone west of UTC, as an operator's psql there may (sqlx itself
/// starts every session in UTC).
async fn west_of_utc(d: &Demo) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("set time zone 'America/New_York'").execute(&mut *c).await?;
                Ok(())
            })
        })
        .connect(&d.db.url)
        .await?;
    let zone: String = sqlx::query_scalar("show timezone").fetch_one(&pool).await?;
    assert_eq!(zone, "America/New_York");
    Ok(pool)
}

/// `ownhealth partition` with extra arguments.
fn cli_args(d: &Demo, rows: &str, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(Command::new(BIN)
        .arg("partition")
        .args(args)
        .env("OWNHEALTH_DATABASE_URL", &d.db.url)
        .env("OWNHEALTH_DATA_DIR", &d.settings.data_dir)
        .env("OWNHEALTH_PARTITION_ROWS", rows)
        .env_remove("OWNHEALTH_PARTITION_ROWS_FILE")
        .env_remove("OWNHEALTH_DATABASE_URL_FILE")
        .env_remove("OWNHEALTH_DB_PASSWORD")
        .env_remove("OWNHEALTH_DB_PASSWORD_FILE")
        .output()?)
}

/// The same group from a push, with no file to retract: the README query shows a row with no
/// document, `ownhealth partition --drop-pushed-from` removes those samples, the rollups stay what a
/// rebuild makes, and the next run converts.
#[tokio::test]
async fn an_oversized_future_group_from_a_push_has_a_way_out() -> R {
    let readme = include_str!("../../../../deploy/README.md");
    assert!(
        readme.contains("ownhealth partition --drop-pushed-from YYYY-MM-DD"),
        "deploy/README.md gives the command for pushed samples"
    );
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let mut c = pool.acquire().await?;
    let push = push_inbox(&mut c, &d).await?;
    let kept: Vec<Sample> = (1..=3)
        .map(|h| hr(now - chrono::Duration::hours(h), 70.0, push))
        .collect();
    store::write(&mut c, d.alex, None, kept, Conflict::Refresh).await?;
    // a phone whose clock ran ahead, in a zone west of UTC: the first hours reach the day before
    sqlx::query(
        "insert into wearable_sample (profile_id, metric, t, source_id, v, tz_offset_min) \
         select $1, 'hr', $2::timestamptz + n * interval '1 second', $3, 60, -300 \
         from generate_series(1, 50001) n",
    )
    .bind(d.alex)
    .bind(cutoff_for(now))
    .bind(push)
    .execute(&mut *c)
    .await?;
    wearables::recompute_rollups(&mut c, d.alex, None, None).await?;
    drop(c);

    let report = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&report), "needs_maintenance", "{report}");
    let note = report["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("pushes"),
        "the note names pushes as well as files: {note}"
    );
    let since = cutoff_for(now).format("%Y-%m-%d").to_string();
    let files = future_files(pool, &since, "wearable_sample").await?;
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(
        (files[0].0, files[0].1.as_deref(), files[0].3),
        (None, None, 50001),
        "{files:?}"
    );

    let out = cli_args(&d, "1000", &["--drop-pushed-from", &since])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let dropped: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(dropped["removed"], 50001, "{dropped}");
    let left: i64 = sqlx::query_scalar("select count(*) from wearable_sample where source_id = $1")
        .bind(push)
        .fetch_one(pool)
        .await?;
    assert_eq!(left, 3, "the samples dated before stay");
    let after = rollups(pool).await?;
    rebuild_all(pool).await?;
    assert_eq!(
        rollups(pool).await?,
        after,
        "the command left the rollups a rebuild makes"
    );
    let r = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&r), "converted", "{r}");
    Ok(())
}

/// `--drop-pushed-from` takes only what inboxes pushed with no file, from the start of the UTC day
/// on, whatever the session's zone, out of the samples set aside too. A file's sample (an inbox can
/// bring files too) and the sample of a source no inbox feeds stay. The rollups of the inbox are rebuilt from what stays, so the day
/// before keeps its own. A date that is not ahead of now is refused.
#[tokio::test]
async fn dropping_pushed_samples_takes_only_pushes_from_the_utc_day_and_keeps_the_day_before() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let now = Utc::now();
    let since = cutoff_for(now);
    let day = since.format("%Y-%m-%d").to_string();
    let hours = chrono::Duration::hours;
    let kept = since - hours(2); // the day before in UTC
    let early = since + hours(2); // still the day before in New York
    let mut c = pool.acquire().await?;
    let push = push_inbox(&mut c, &d).await?;
    let band = store::device_source(&mut c, d.alex, "apple_health", Some("Band"), None, None).await?;
    drop(c);
    let doc = document(&d, b"a watch export dated ahead", "application/x-sqlite3").await?;
    let mut c = pool.acquire().await?;
    let pushed = vec![
        hr(kept, 70.0, push),
        hr(early, 71.0, push),
        hr(since + chrono::Duration::days(1), 72.0, push),
        hr(since + hours(4), 73.0, band),
    ];
    store::write(&mut c, d.alex, None, pushed, Conflict::Refresh).await?;
    store::write(
        &mut c,
        d.alex,
        Some(doc),
        vec![hr(since + hours(3), 74.0, push)],
        Conflict::Refresh,
    )
    .await?;
    drop(c);
    // the demo seed writes rollups of its own: start from those a rebuild makes
    rebuild_all(pool).await?;
    let prepared = partition::prepare(pool, &opts(now)).await?;
    assert_eq!(state(&prepared), "prepared", "{prepared}");
    assert_eq!(prepared["held_aside"], 4, "{prepared}");

    let west = west_of_utc(&d).await?;
    let files = future_files(&west, &day, "wearable_sample_ahead").await?;
    let counts: Vec<(Option<Uuid>, i64)> = files.iter().map(|f| (f.0, f.3)).collect();
    assert_eq!(
        counts,
        vec![(None, 3), (Some(doc), 1)],
        "the README query reads the UTC day"
    );
    let r = partition::drop_pushed(&west, &opts(now), since.date_naive()).await?;
    west.close().await;

    let left: Vec<(i16, DateTime<Utc>)> = sqlx::query_as(
        "select source_id, t from wearable_sample where profile_id = $1 and t >= $2 \
         union all select source_id, t from wearable_sample_ahead where profile_id = $1 and t >= $2 order by 2",
    )
    .bind(d.alex)
    .bind(kept)
    .fetch_all(pool)
    .await?;
    assert_eq!(
        left,
        vec![(push, kept), (push, since + hours(3)), (band, since + hours(4))],
        "only the pushes from the UTC day on are gone"
    );
    let inbox_rollups: Vec<(String, DateTime<Utc>, i32)> =
        sqlx::query_as("select bucket, t, n from wearable_rollup where source_id = $1 order by bucket, t")
            .bind(push)
            .fetch_all(pool)
            .await?;
    assert_eq!(
        inbox_rollups,
        vec![
            ("day".to_string(), since - chrono::Duration::days(1), 1),
            ("day".to_string(), since, 1),
            ("hour".to_string(), kept, 1),
            ("hour".to_string(), since + hours(3), 1),
        ],
        "the day before keeps its rollups, and the file's sample its own"
    );
    let after = rollups(pool).await?;
    rebuild_all(pool).await?;
    assert_eq!(
        rollups(pool).await?,
        after,
        "the command left the rollups a rebuild makes"
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["state"], "dropped", "{r}");
    assert_eq!(r["from"], since.to_rfc3339(), "{r}");
    assert_eq!(r["removed"], 2, "{r}");
    assert_eq!(r["removed_set_aside"], 2, "{r}");

    let converted = partition::run(pool, &opts(now)).await?;
    assert_eq!(state(&converted), "converted", "{converted}");
    let back: i64 = sqlx::query_scalar("select count(*) from wearable_sample where t >= $1")
        .bind(since)
        .fetch_one(pool)
        .await?;
    assert_eq!(back, 2, "the file's sample and the band's come back");

    let refused = partition::drop_pushed(pool, &opts(now), now.date_naive()).await?;
    assert_eq!(state(&refused), "refused", "{refused}");
    assert_eq!(refused["ok"], false, "{refused}");
    let still: i64 = sqlx::query_scalar("select count(*) from wearable_sample where source_id = $1")
        .bind(push)
        .fetch_one(pool)
        .await?;
    assert!(still >= 1, "a refused date deletes nothing");
    Ok(())
}

#[tokio::test]
async fn deleting_a_profile_as_the_app_cascades_to_its_private_staged_samples() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let role = TestLogin::new(&d.db, None).await?;
    let outcome = async {
        role.grant_app_rights(&d.db).await?;
        let app = role.connect(&d.db).await?;
        let other: Uuid = sqlx::query_scalar("select id from profile where id <> $1 order by slug limit 1")
            .bind(d.alex)
            .fetch_one(pool)
            .await?;
        let now = Utc::now();
        let far = cutoff_for(now);
        let mut c = pool.acquire().await?;
        for profile in [d.alex, other] {
            let source = watch(&mut c, profile).await?;
            store::write(&mut c, profile, None, vec![hr(far, 60.0, source)], Conflict::Keep).await?;
        }
        drop(c);
        assert_eq!(partition::prepare(pool, &opts(now)).await?["held_aside"], 2);
        // This caller can delete profiles but cannot see staging through information_schema.
        sqlx::query(AssertSqlSafe(format!(
            "revoke all on wearable_sample_ahead from {}",
            role.name
        )))
        .execute(pool)
        .await?;
        let mut tx = app.begin().await?;
        ownhealth_auth::household::delete_profile(&mut tx, d.alex).await?;
        tx.commit().await?;
        app.close().await;
        let remaining: Vec<Uuid> = sqlx::query_scalar("select profile_id from wearable_sample_ahead")
            .fetch_all(pool)
            .await?;
        Ok::<_, anyhow::Error>((remaining, other))
    }
    .await;
    let (remaining, other) = outcome?;
    assert_eq!(remaining, [other]);
    Ok(())
}

#[tokio::test]
async fn partitioning_does_not_inherit_new_table_default_grants() -> R {
    let d = demo().await?;
    let pool = &d.db.pool;
    let role = TestLogin::new(&d.db, None).await?;
    let outcome = async {
        let name = &role.name;
        let now = Utc::now();
        let mut c = pool.acquire().await?;
        let source = watch(&mut c, d.alex).await?;
        store::write(
            &mut c,
            d.alex,
            None,
            vec![hr(cutoff_for(now), 60.0, source)],
            Conflict::Keep,
        )
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "alter default privileges grant select on tables to {name}"
        )))
        .execute(&mut *c)
        .await?;
        drop(c);
        let readable = |table: &'static str| async move {
            sqlx::query_scalar::<_, bool>("select has_table_privilege($1, $2, 'SELECT')")
                .bind(name)
                .bind(table)
                .fetch_one(pool)
                .await
        };
        let before = readable("wearable_sample").await?;
        assert_eq!(state(&partition::prepare(pool, &opts(now)).await?), "prepared");
        let staged = readable("wearable_sample_ahead").await?;
        assert_eq!(state(&partition::run(pool, &opts(now)).await?), "converted");
        let parent = readable("wearable_sample").await?;
        let default = readable("wearable_sample_default").await?;
        let month: bool = sqlx::query_scalar("select has_table_privilege($1, $2, 'SELECT')")
            .bind(name)
            .bind(month_name(cutoff_for(now)))
            .fetch_one(pool)
            .await?;
        Ok::<_, anyhow::Error>([before, staged, parent, default, month])
    }
    .await;
    assert_eq!(outcome?, [false; 5], "a reporting role gained wearable access");
    Ok(())
}
