//! Migration 1110 (family history) on a 0.3.0 database: the 0.3.0 schema with the 0.3.0 demo seed,
//! then the v0.3 migrations, then the family demo rows. Every profile's state_hash stays the same at
//! each step: nothing of the family history enters the hashed state body.

use std::collections::{BTreeMap, HashMap};

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{DemoContext, as_of, create_rivera_family};
use ownhealth_core::migrate::{embedded, migrate};
use ownhealth_core::testing::TestDb;
use sqlx::PgConnection;
use uuid::Uuid;

/// The last migration 0.3.0 shipped.
const V030: i32 = 1052;

async fn hashes(
    conn: &mut PgConnection,
    profiles: &BTreeMap<String, Uuid>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (slug, id) in profiles {
        let body = ownhealth_rules::build_state(&mut *conn, *id, as_of(), None).await?;
        out.insert(
            slug.clone(),
            body["state_hash"].as_str().unwrap_or_default().to_string(),
        );
    }
    Ok(out)
}

#[tokio::test]
async fn a_030_database_migrates_and_gains_relatives_with_every_state_hash_kept() -> anyhow::Result<()> {
    let mut db = TestDb::empty().await?;
    let all = embedded()?;
    let old: Vec<_> = all.iter().filter(|m| m.version <= V030).cloned().collect();
    assert!(old.len() < all.len(), "1110 is not among the 0.3.0 files");
    migrate(&db.pool, &old).await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    let mut tx = db.pool.begin().await?;
    let family = create_rivera_family(&mut tx, "Rivera (0.3.0)").await?;
    let mut ctx = DemoContext {
        settings,
        blobs,
        family,
        as_of: as_of(),
        documents: HashMap::new(),
        src_ids: HashMap::new(),
    };
    ownhealth_ops::seed_base(&mut tx, &mut ctx).await?;
    tx.commit().await?;
    let profiles: BTreeMap<String, Uuid> = ctx.family.profiles.clone().into_iter().collect();
    let mut c = db.pool.acquire().await?;
    let before = hashes(&mut c, &profiles).await?;
    assert_eq!(before.len(), 4);
    assert!(before.values().all(|h| h.starts_with("sha256:")), "{before:?}");
    drop(c);

    let applied = migrate(&db.pool, &all).await?;
    assert!(applied.contains(&1110), "{applied:?}");
    // The server restarts after a migration. A connection from before it holds plans for the old
    // medication_statement row type (1100 adds columns), and Postgres refuses to reuse them ("cached
    // plan must not change result type"), so everything after the upgrade uses new connections.
    db.pool.close().await;
    db.pool = sqlx::PgPool::connect(&db.url).await?;
    let mut c = db.pool.acquire().await?;
    assert_eq!(
        hashes(&mut c, &profiles).await?,
        before,
        "the migration moved a state_hash"
    );

    let mut tx = db.pool.begin().await?;
    ownhealth_ops::seed_family(&mut tx, &mut ctx).await?;
    tx.commit().await?;
    let n: i64 = sqlx::query_scalar("select count(*) from family_member")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(n, 7);
    assert_eq!(
        hashes(&mut c, &profiles).await?,
        before,
        "family rows moved a state_hash"
    );
    Ok(())
}
