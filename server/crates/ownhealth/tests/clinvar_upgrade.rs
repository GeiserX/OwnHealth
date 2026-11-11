//! Migration 1120 (the ClinVar diff) on a 0.3.0 database: the 0.3.0 schema with the 0.3.0 demo seed,
//! then the v0.3 migrations, then a ClinVar diff. Every profile's state_hash stays the same at each
//! step: no ClinVar snapshot or event enters the hashed state body. Then a profile with snapshots
//! and events is deleted. All data is synthetic.

use std::collections::BTreeMap;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{as_of, seed_demo};
use ownhealth_core::migrate::{embedded, migrate};
use ownhealth_core::testing::TestDb;
use sqlx::PgConnection;
use sqlx::types::Uuid;

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

async fn count(conn: &mut PgConnection, sql: &str, profile: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(profile)
        .fetch_one(conn)
        .await?)
}

const HEAD: &str = "##fileformat=VCFv4.1\n##fileDate=20261005\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n";

#[tokio::test]
async fn a_030_database_migrates_diffs_clinvar_and_deletes_a_profile_with_every_state_hash_kept() -> anyhow::Result<()>
{
    ownhealth::register_lanes();
    let db = TestDb::empty().await?;
    let all = embedded()?;
    let old: Vec<_> = all.iter().filter(|m| m.version <= V030).cloned().collect();
    assert!(old.len() < all.len(), "1120 is not among the 0.3.0 files");
    migrate(&db.pool, &old).await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    let profiles: BTreeMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>("select slug, id from profile")
        .fetch_all(&db.pool)
        .await?
        .into_iter()
        .collect();
    let mut c = db.pool.acquire().await?;
    let before = hashes(&mut c, &profiles).await?;
    assert_eq!(before.len(), 4);
    assert!(before.values().all(|h| h.starts_with("sha256:")), "{before:?}");

    let applied = migrate(&db.pool, &all).await?;
    assert!(applied.contains(&1120), "{applied:?}");
    assert_eq!(
        hashes(&mut c, &profiles).await?,
        before,
        "the migration moved a state_hash"
    );

    // A release that downgrades Alex's and Lucía's CFTR (3 stars in the facts) and upgrades his MTHFR.
    let release = dir.path().join("clinvar.vcf");
    std::fs::write(
        &release,
        format!(
            "{HEAD}1\t11796321\t900030\tG\tA\t.\t.\tCLNREVSTAT=criteria_provided,_single_submitter;\
             CLNSIG=Likely_pathogenic;GENEINFO=MTHFR:4524\n\
             7\t117559590\t7105\tATCT\tA\t.\t.\tCLNREVSTAT=criteria_provided,_single_submitter;\
             CLNSIG=Uncertain_significance;GENEINFO=CFTR:1080\n"
        ),
    )?;
    let report = ownhealth_genome::clinvar::diff_release(&db.pool, &blobs, &release, 18).await?;
    assert_eq!(report.datasets.len(), 3, "{report:?}");
    let alex = profiles["alex"];
    let kinds: Vec<(String, bool)> =
        sqlx::query_as("select kind, alerted from genome_event where profile_id = $1 order by variation_id")
            .bind(alex)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(
        kinds,
        vec![
            ("downgraded_from_plp".to_string(), true),
            ("upgraded_to_plp".to_string(), true)
        ]
    );
    assert_eq!(
        hashes(&mut c, &profiles).await?,
        before,
        "a ClinVar diff moved a state_hash"
    );

    // Deleting Alex goes through his snapshots, events and alerts.
    let snap = "select count(*) from clinvar_carried c join genome_dataset d on d.id = c.dataset_id \
                where d.profile_id = $1";
    assert_eq!(count(&mut c, snap, alex).await?, 2);
    let mut tx = db.pool.begin().await?;
    ownhealth_auth::household::delete_profile(&mut tx, alex).await?;
    tx.commit().await?;
    assert_eq!(count(&mut c, snap, alex).await?, 0);
    assert_eq!(
        count(&mut c, "select count(*) from genome_event where profile_id = $1", alex).await?,
        0
    );
    let left: i64 = sqlx::query_scalar("select count(*) from genome_event")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(left, 1, "Lucía's event stays");
    Ok(())
}
