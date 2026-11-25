//! Every importer's `wearable_source.provider` has its line in `data/connectors/providers.yaml`, stored
//! and served: these are the person's own files and pushes, so agents read them as before. A provider
//! the file does not name is withheld and refused at ingest.

mod common;

use chrono::Utc;
use common::{Env, R};
use ownhealth_core::providers::{Ingest, Mcp, policy};
use ownhealth_devices::store::{self, Conflict, Sample};

#[test]
fn every_importer_has_a_served_line_in_the_provider_policy() {
    let importers = [
        ownhealth_ingest_structured::apple_health::PROVIDER,
        ownhealth_devices::hae::PROVIDER,
        ownhealth_devices::gadgetbridge::PROVIDER,
        ownhealth_devices::nightscout::PROVIDER,
        ownhealth_devices::fit::PROVIDER,
        ownhealth_devices::garmin_zip::PROVIDER,
        ownhealth_devices::strava_zip::PROVIDER,
        ownhealth_devices::takeout_fitbit::PROVIDER,
        ownhealth_devices::cgm_csv::LIBREVIEW,
        ownhealth_devices::cgm_csv::CLARITY,
    ];
    for p in importers {
        let line = policy().providers.get(p);
        assert!(
            line.is_some_and(|l| l.ingest == Ingest::Allow && l.mcp == Mcp::Served),
            "{p}: {line:?}"
        );
    }
}

#[tokio::test]
async fn a_sample_batch_with_a_disallowed_provider_is_rejected_before_writing() -> R {
    let env = Env::new().await?;
    let profile = env.fam.profile("sam");
    let mut conn = env.db.pool.acquire().await?;
    let allowed = store::device_source(&mut conn, profile, "oura", Some("Ring"), None, None).await?;
    let sample = |source| Sample {
        metric: "hr",
        t: Utc::now(),
        t_end: None,
        v: 64.0,
        tz: None,
        source,
    };
    for provider in ["strava", "new_vendor"] {
        // Seed a source that could already exist in an older database.
        let denied: i16 =
            sqlx::query_scalar("insert into wearable_source (profile_id, provider) values ($1, $2) returning id")
                .bind(profile)
                .bind(provider)
                .fetch_one(&mut *conn)
                .await?;
        let result = store::write(
            &mut conn,
            profile,
            None,
            vec![sample(allowed), sample(denied)],
            Conflict::Keep,
        )
        .await;
        let stored: i64 = sqlx::query_scalar("select count(*) from wearable_sample where source_id = any($1)")
            .bind(&[allowed, denied][..])
            .fetch_one(&mut *conn)
            .await?;
        assert_eq!(stored, 0, "{provider}: {result:?}");
        assert!(
            matches!(&result, Err(ownhealth_core::Error::WearableIngestDenied { provider: denied }) if denied == provider),
            "{provider}: {result:?}"
        );
    }
    assert_eq!(
        store::write(&mut conn, profile, None, vec![sample(allowed)], Conflict::Keep)
            .await?
            .new,
        1,
    );
    Ok(())
}
