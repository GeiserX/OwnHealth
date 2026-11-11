//! The medication lane (migration 1100) leaves the state body alone: an upgrade from the 0.3.0 schema
//! keeps every state_hash, and a medication that gains a schedule, a prescriber or codes hashes the same.

mod common;

use common::{EXPECTED, FIXTURES, Seeded, as_of, install_fake_genome, seeded};
use ownhealth_core::migrate::{embedded, migrate, migrate_embedded};
use ownhealth_core::testing::TestDb;
use ownhealth_rules::{build_state, state_hash};

type R = anyhow::Result<()>;

const SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];
/// The last migration 0.3.0 shipped.
const V0_3_0: i32 = 1052;

async fn hashes(s: &Seeded) -> anyhow::Result<Vec<(String, String)>> {
    let mut tx = s.pool().begin().await?;
    let mut out = Vec::new();
    for slug in SLUGS {
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        out.push((slug.to_string(), state_hash(&body)?));
    }
    Ok(out)
}

fn expected() -> Vec<(String, String)> {
    SLUGS
        .iter()
        .map(|s| {
            (
                s.to_string(),
                EXPECTED["bodies"][s]["state_hash"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn upgrading_a_0_3_0_database_keeps_every_state_hash() -> R {
    install_fake_genome();
    // the stand-in genome findings of the fixture, copied from a seeded database
    let reference = seeded().await?;
    let db = TestDb::empty().await?;
    let old: Vec<_> = embedded()?.into_iter().filter(|m| m.version <= V0_3_0).collect();
    migrate(&db.pool, &old).await?;
    let has_column: bool = sqlx::query_scalar(
        "select exists (select 1 from information_schema.columns where table_name = 'medication_statement' and column_name = 'codes')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(!has_column, "the fixture database is the 0.3.0 schema");
    let rows = std::fs::read_to_string(format!("{FIXTURES}/seed.sql"))?;
    let script = format!("begin; set local session_replication_role = replica;\n{rows}\ncommit;");
    sqlx::raw_sql(sqlx::AssertSqlSafe(script)).execute(&db.pool).await?;
    let mut s = Seeded { db };
    for slug in SLUGS {
        s.set_findings(slug, reference.findings(slug));
    }
    assert_eq!(hashes(&s).await?, expected(), "before the upgrade");
    let applied = migrate_embedded(s.pool()).await?;
    assert!(applied.contains(&1100), "{applied:?}");
    // The server restarts after a migration. A connection from before it holds plans for the old
    // medication_statement row type, and Postgres refuses to reuse them ("cached plan must not change
    // result type"), so the hashes after the upgrade are read through new connections.
    s.db.pool.close().await;
    s.db.pool = sqlx::PgPool::connect(&s.db.url).await?;
    assert_eq!(hashes(&s).await?, expected(), "after the upgrade");
    let filled: i64 = sqlx::query_scalar(
        "select count(*) from medication_statement where schedule_text is not null or prescriber is not null or codes is not null",
    )
    .fetch_one(s.pool())
    .await?;
    assert_eq!(filled, 0, "old rows read the new fields as null");
    Ok(())
}

#[tokio::test]
async fn a_schedule_prescriber_or_codes_do_not_move_the_hash() -> R {
    let s = seeded().await?;
    assert_eq!(hashes(&s).await?, expected());
    // Same rows, same src ids; only the new fields differ. The lifecycle trigger forbids this update in
    // the app, so the test writes it as the seed does.
    sqlx::raw_sql(
        "begin; set local session_replication_role = replica; \
         update medication_statement set schedule_text = 'every evening', prescriber = 'Dr. Synthetic', \
         codes = '[{\"system\": \"http://www.whocc.no/atc\", \"code\": \"C10AA05\"}]'; commit;",
    )
    .execute(s.pool())
    .await?;
    let n: i64 = sqlx::query_scalar("select count(*) from medication_statement where schedule_text is not null")
        .fetch_one(s.pool())
        .await?;
    assert!(n >= 3, "every seeded medication now has a schedule");
    assert_eq!(hashes(&s).await?, expected());
    Ok(())
}
