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

/// The locale a login keeps is a registry language (`ownhealth_core::langs::LANGS`); any other code
/// becomes the default. The database takes every planned code, so a new language needs no migration.
#[tokio::test]
async fn a_login_keeps_a_registered_language_and_the_database_takes_every_planned_one() -> R {
    use ownhealth_core::langs::{LANGS, LOCALES};
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let o = setup_owner(&mut c, &owner("Ana")).await?;
    let stored = |id| sqlx::query_scalar::<_, String>("select locale from app_user where id = $1").bind(id);
    for lang in LANGS {
        let (id, _) = invite_member(&mut c, o.household_id, o.user_id, "Luis", lang, None).await?;
        assert_eq!(stored(id).fetch_one(&mut *c).await?, lang);
        let fresh = TestDb::new().await?;
        let mut f = fresh.pool.acquire().await?;
        let first = setup_owner(
            &mut f,
            &OwnerIn {
                locale: lang,
                ..owner("Carmen")
            },
        )
        .await?;
        assert_eq!(stored(first.user_id).fetch_one(&mut *f).await?, lang);
    }
    for other in ["xx", "", "ES", "es-ES"] {
        let (id, _) = invite_member(&mut c, o.household_id, o.user_id, "Luis", other, None).await?;
        assert_eq!(stored(id).fetch_one(&mut *c).await?, LANGS[0], "{other:?}");
    }
    for code in LOCALES {
        sqlx::query("insert into app_user (household_id, display_name, locale) values ($1, 'Sam', $2)")
            .bind(o.household_id)
            .bind(code)
            .execute(&mut *c)
            .await
            .map_err(|e| anyhow::anyhow!("app_user.locale refuses {code}: {e}"))?;
    }
    Ok(())
}
