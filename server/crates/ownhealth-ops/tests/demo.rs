//! The base demo seed (`tests/test_demo.py`).

use ownhealth_core::access::{Principal, effective_permission, today_utc};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{DEMO_HOUSEHOLD, as_of, seed_demo};
use ownhealth_core::testing::TestDb;
use ownhealth_rules::{calc, family};
use rust_decimal::Decimal;
use uuid::Uuid;

#[tokio::test]
async fn demo_seed_matches_the_alex_fixture() -> anyhow::Result<()> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::for_settings(&settings)?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    assert!(!seed_demo(&db.pool, &settings, &blobs, false).await?); // once
    let p = &db.pool;

    let (value, interp, document_id): (Decimal, String, Uuid) =
        sqlx::query_as("select value_num, interpretation, document_id from observation where src_id = 'src_a12'")
            .fetch_one(p)
            .await?;
    assert_eq!((value, interp.as_str()), (Decimal::from(24), "L"));
    let lpa: Decimal = sqlx::query_scalar("select value_num from observation where src_id = 'src_a07'")
        .fetch_one(p)
        .await?;
    assert_eq!(lpa, Decimal::from(142));
    let ldl: i64 = sqlx::query_scalar(
        "select count(*) from observation o join profile p on p.id = o.profile_id \
         where p.slug = 'alex' and alias_group = 'ldl_c'",
    )
    .fetch_one(p)
    .await?;
    assert_eq!(ldl, 9);
    let pages: i64 = sqlx::query_scalar("select count(*) from document_page where document_id = $1")
        .bind(document_id)
        .fetch_one(p)
        .await?;
    assert_eq!(pages, 2);
    let open: i64 = sqlx::query_scalar(
        "select count(*) from review_item r join household h on h.id = r.household_id \
         where h.name = $1 and r.status = 'open'",
    )
    .bind(DEMO_HOUSEHOLD)
    .fetch_one(p)
    .await?;
    assert_eq!(open, 7);
    let active: Vec<String> = sqlx::query_scalar(
        "select name from medication_statement where src_id in ('src_m02', 'src_m05') \
         and med_status = 'active' order by name",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(active, ["Atorvastatin", "Cenobamate"]);
    let metrics: Vec<String> = sqlx::query_scalar(
        "select distinct r.metric from wearable_rollup r join profile p on p.id = r.profile_id \
         where p.slug = 'alex' order by 1",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(metrics, ["hr_resting", "hrv_rmssd", "sleep_asleep", "steps", "vo2max"]);
    // every observation keeps provenance with a page and a bbox
    let missing: i64 = sqlx::query_scalar(
        "select count(*) from observation o join src s on s.id = o.src_id \
         where s.kind = 'document_region' and (s.page_no is null or s.bbox is null)",
    )
    .fetch_one(p)
    .await?;
    assert_eq!(missing, 0);

    // the wearable values Python's random.Random(20261107) gave: the first and last samples
    let first: Vec<f32> = sqlx::query_scalar(
        "select v from wearable_sample s join profile p on p.id = s.profile_id \
         where p.slug = 'alex' and metric = 'hr_resting' order by t limit 3",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(first, [58.2, 57.6, 58.8]);
    let samples: i64 = sqlx::query_scalar("select count(*) from wearable_sample")
        .fetch_one(p)
        .await?;
    assert_eq!(samples, 373);
    // the named sources other hooks cite all exist
    let named: i64 = sqlx::query_scalar(
        "select count(*) from src where id in ('src_a12', 'src_a07', 'src_a05', 'src_m02', 'src_m05', 'src_n41')",
    )
    .fetch_one(p)
    .await?;
    assert_eq!(named, 6);
    // Alex's traumatology visit with no report, typed by hand
    let visits: Vec<(String, String)> = sqlx::query_as(
        "select to_char(e.period_start at time zone 'UTC', 'YYYY-MM-DD'), s.kind from encounter e \
         join src s on s.id = e.src_id join profile p on p.id = e.profile_id where p.slug = 'alex'",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(visits, [("2025-11-10".to_string(), "manual".to_string())]);
    // and one dated report that is not a lab report, so the timeline shows a document event
    let docs: Vec<(String, String, String)> = sqlx::query_as(
        "select d.doc_type, d.filename, to_char(d.collected_at at time zone 'UTC', 'YYYY-MM-DD') from document d \
         join profile p on p.id = d.profile_id where p.slug = 'alex' and d.doc_type = 'ecg'",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(
        docs,
        [(
            "ecg".to_string(),
            "ECG_Hospital_Norte_2024-02-14.pdf".to_string(),
            "2024-02-14".to_string()
        )]
    );
    Ok(())
}

#[tokio::test]
async fn a_forced_second_seed_gets_generated_src_ids() -> anyhow::Result<()> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::for_settings(&settings)?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    assert!(seed_demo(&db.pool, &settings, &blobs, true).await?);
    let households: i64 = sqlx::query_scalar("select count(*) from household where name = $1")
        .bind(DEMO_HOUSEHOLD)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(households, 2);
    let a12: i64 = sqlx::query_scalar("select count(*) from src where id = 'src_a12'")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(a12, 1);
    // each household's vitamin D proposal cites its own household's source, not the first seed's src_a12
    let own: Vec<(bool, bool)> = sqlx::query_as(
        "select s.id = 'src_a12', p.household_id = r.household_id from review_item r \
         join src s on s.id = r.context_src_ids[1] join profile p on p.id = s.profile_id \
         where r.kind = 'agent_proposal' and r.payload->'payload'->>'text' like 'Ask about a winter vitamin D%' \
         and r.payload->'payload'->>'attach_to' = s.id and r.payload->'based_on'->>0 = s.id \
         order by r.created_at",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(own, vec![(true, true), (false, true)]);
    Ok(())
}

async fn fresh_demo() -> anyhow::Result<(TestDb, tempfile::TempDir)> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::for_settings(&settings)?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    Ok((db, dir))
}

#[tokio::test]
async fn fresh_demo_computes_score2_and_prevent() -> anyhow::Result<()> {
    let (db, _dir) = fresh_demo().await?;
    let mut conn = db.pool.acquire().await?;
    let alex: Uuid = sqlx::query_scalar("select id from profile where slug = 'alex'")
        .fetch_one(&mut *conn)
        .await?;
    let document = calc::document(&mut conn, alex, as_of()).await?;
    let items = document["items"].as_array().expect("calculator items are an array");
    let risks: Vec<_> = items
        .iter()
        .filter(|item| matches!(item["id"].as_str(), Some("score2" | "prevent_cvd" | "prevent_ascvd")))
        .collect();

    assert_eq!(risks.len(), 3);
    assert!(
        risks
            .iter()
            .all(|item| item["status"] == "ok" && item["value"].is_string()),
        "{risks:#?}"
    );
    Ok(())
}

/// Lucía's guardian sees the hint her mother's shared LDL feeds. Read as of today, as the Family
/// page and the MCP status read it: the seed records its rows when it runs, after the demo date
/// (`as_of()`), so a read as of that date sees no relative and no share yet.
#[tokio::test]
async fn fresh_demo_shows_lucia_a_hint_from_her_mothers_shared_ldl() -> anyhow::Result<()> {
    let (db, _dir) = fresh_demo().await?;
    let mut conn = db.pool.acquire().await?;
    let (lucia, household, guardian): (Uuid, Uuid, Uuid) = sqlx::query_as(
        "select p.id, p.household_id, g.user_id from profile p join profile_grant g on g.profile_id = p.id \
         join app_user u on u.id = g.user_id where p.slug = 'lucia' and g.role = 'guardian' \
         and u.display_name = 'Alex Rivera'",
    )
    .fetch_one(&mut *conn)
    .await?;
    let principal = Principal::session(guardian, household);
    let perm = effective_permission(&mut conn, &principal, lucia).await?;
    let hints = family::hints(&mut conn, &principal, &perm, today_utc()).await?;

    assert!(
        hints
            .iter()
            .any(|hint| hint["rule"] == "lab.ldl_fh" && hint["rel"] == "MTH"),
        "{hints:#?}"
    );
    Ok(())
}

/// The demo's one share is a consent a demo user made and can take back: its source's manager
/// granted it, and the audit log holds the consent change, as `PUT /family-shares` writes them.
#[tokio::test]
async fn the_demo_share_is_made_by_its_source_manager() -> anyhow::Result<()> {
    let (db, _dir) = fresh_demo().await?;
    let mut conn = db.pool.acquire().await?;
    let shares: Vec<(Uuid, Uuid, Vec<String>, Option<Uuid>)> =
        sqlx::query_as("select source_profile_id, target_profile_id, categories, granted_by from family_share")
            .fetch_all(&mut *conn)
            .await?;
    assert_eq!(shares.len(), 1, "{shares:?}");
    let (source, target, categories, granted_by) = shares[0].clone();
    assert_eq!(categories, ["labs"]);
    let granted_by = granted_by.expect("the share names who granted it");
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(source)
        .fetch_one(&mut *conn)
        .await?;
    let perm = effective_permission(&mut conn, &Principal::session(granted_by, household), source).await?;
    assert!(ownhealth_auth::household::can_manage(&perm), "role {}", perm.role);
    type Row = (Option<Uuid>, Option<Vec<String>>, Option<String>);
    let audit: Vec<Row> = sqlx::query_as(
        "select actor_id, resource_ids, args_summary from audit_event \
         where tool = 'family_share' and action = 'consent_change' and profile_id = $1",
    )
    .bind(source)
    .fetch_all(&mut *conn)
    .await?;
    assert_eq!(
        audit,
        vec![(
            Some(granted_by),
            Some(vec![target.to_string()]),
            Some("categories=labs".into())
        )]
    );
    Ok(())
}

/// The docs' stale-TSH example (architecture.md section 16): Alex's last TSH is `src_a05`.
#[tokio::test]
async fn fresh_demo_keeps_alex_tsh_stale() -> anyhow::Result<()> {
    let (db, _dir) = fresh_demo().await?;
    let last: (Option<String>, String) = sqlx::query_as(
        "select o.src_id, to_char(o.effective_at, 'YYYY-MM-DD') from observation o \
         join profile p on p.id = o.profile_id where p.slug = 'alex' and o.alias_group = 'tsh' \
         order by o.effective_at desc limit 1",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(last, (Some("src_a05".into()), "2025-01-20".into()));
    Ok(())
}

/// A dashboard change compares a profile's last two LDL draws; in the demo both are one method.
#[tokio::test]
async fn demo_ldl_changes_compare_one_method() -> anyhow::Result<()> {
    let (db, _dir) = fresh_demo().await?;
    let series: Vec<(String, Vec<String>)> = sqlx::query_as(
        "select p.slug, array_agg(coalesce(o.loinc, '') order by o.effective_at desc) from observation o \
         join profile p on p.id = o.profile_id where o.alias_group = 'ldl_c' group by p.slug order by 1",
    )
    .fetch_all(&db.pool)
    .await?;
    for (slug, loincs) in &series {
        if let [last, before, ..] = loincs.as_slice() {
            assert_eq!(last, before, "{slug}: {loincs:?}");
        }
    }
    Ok(())
}
