//! Household rules that need Postgres: nobody leaves a profile without a manager.

use chrono::{Duration, Utc};
use ownhealth_auth::household::{OwnerIn, invite_member, orphaned_profiles, setup_owner};
use ownhealth_core::access::set_grant;
use ownhealth_core::testing::TestDb;

type R = anyhow::Result<()>;

fn owner(name: &str) -> OwnerIn<'_> {
    OwnerIn {
        household_name: "Home",
        display_name: name,
        locale: "es",
        ..OwnerIn::default()
    }
}

#[tokio::test]
async fn an_expired_guardian_does_not_count_as_another_manager() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let o = setup_owner(&mut c, &owner("Ana")).await?;
    let (other, _) = invite_member(&mut c, o.household_id, o.user_id, "Luis", "es", None).await?;
    let past = Utc::now() - Duration::days(1);
    set_grant(
        &mut c,
        other,
        o.profile_id,
        "guardian",
        None,
        Some(o.user_id),
        Some(past),
    )
    .await?;
    assert_eq!(orphaned_profiles(&mut c, o.user_id, None).await?, vec![o.slug.clone()]);
    let future = Utc::now() + Duration::days(1);
    set_grant(
        &mut c,
        other,
        o.profile_id,
        "guardian",
        None,
        Some(o.user_id),
        Some(future),
    )
    .await?;
    assert!(orphaned_profiles(&mut c, o.user_id, None).await?.is_empty());
    Ok(())
}
