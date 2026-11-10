//! Ownership transfer at majority: a child's profile, past the household's age of majority, is
//! claimed by its person; the guardians' access ends then and her genome switches start off. The
//! household sets its own age of majority.

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeMap;

use axum::http::StatusCode;
use chrono::{Local, Months, NaiveDate};
use ownhealth_auth::household;
use serde_json::{Value, json};
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

fn day(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
}

async fn exec(env: &Env, sql: &str, profile: Uuid) -> R {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(profile)
        .execute(env.pool())
        .await?;
    Ok(())
}

async fn grants(env: &Env, profile: Uuid) -> anyhow::Result<BTreeMap<Uuid, String>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as("select user_id, role from profile_grant where profile_id = $1")
        .bind(profile)
        .fetch_all(env.pool())
        .await?;
    Ok(rows.into_iter().collect())
}

async fn audits(env: &Env, profile: Uuid) -> anyhow::Result<Vec<(Option<Uuid>, Vec<String>, String)>> {
    Ok(sqlx::query_as(
        "select actor_id, resource_ids, args_summary from audit_event \
         where profile_id = $1 and action = 'grant_change' order by id",
    )
    .bind(profile)
    .fetch_all(env.pool())
    .await?)
}

/// Lucía born 2008-06-21, Alex her guardian, Sam her caregiver, both genome switches on; her new
/// login claims the profile on `on`.
async fn claim_on(on: &str) -> anyhow::Result<(Env, Uuid)> {
    let env = Env::new(true).await?;
    let lucia = env.profile("lucia");
    exec(
        &env,
        "update profile set birth_date = '2008-06-21', genome_sensitive_ui = true, \
         genome_sensitive_agents = true where id = $1",
        lucia,
    )
    .await?;
    exec(
        &env,
        &format!(
            "update profile_grant set role = 'caregiver' where profile_id = $1 and user_id = '{}'",
            env.user("sam")
        ),
        lucia,
    )
    .await?;
    let mut tx = env.pool().begin().await?;
    let (user, _) = household::invite_member(
        &mut tx,
        env.fam().household_id,
        env.user("alex"),
        "Lucía Rivera",
        "es",
        Some(lucia),
    )
    .await?;
    household::claim_profile(&mut tx, lucia, user, Some(env.user("alex")), day(on)).await?;
    tx.commit().await?;
    Ok((env, user))
}

async fn switches(env: &Env, profile: Uuid) -> anyhow::Result<(bool, bool)> {
    Ok(
        sqlx::query_as("select genome_sensitive_ui, genome_sensitive_agents from profile where id = $1")
            .bind(profile)
            .fetch_one(env.pool())
            .await?,
    )
}

#[tokio::test]
async fn a_claim_the_day_before_majority_keeps_the_guardians() -> R {
    let (env, lucia_user) = claim_on("2026-06-20").await?;
    let lucia = env.profile("lucia");
    let expected = BTreeMap::from([
        (env.user("alex"), "guardian".to_string()),
        (env.user("sam"), "caregiver".to_string()),
        (lucia_user, "owner".to_string()),
    ]);
    assert_eq!(grants(&env, lucia).await?, expected);
    assert_eq!(switches(&env, lucia).await?, (true, true));
    assert!(audits(&env, lucia).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_claim_at_majority_ends_guardian_access() -> R {
    for on in ["2026-06-21", "2026-06-22"] {
        let (env, lucia_user) = claim_on(on).await?;
        let lucia = env.profile("lucia");
        let expected = BTreeMap::from([
            (env.user("sam"), "caregiver".to_string()),
            (lucia_user, "owner".to_string()),
        ]);
        assert_eq!(grants(&env, lucia).await?, expected, "{on}");
        assert_eq!(switches(&env, lucia).await?, (false, false), "{on}");
        // one row per guardian whose access ended, on her profile, written as her own act
        let rows = audits(&env, lucia).await?;
        assert_eq!(rows.len(), 1, "{on}");
        let (actor, ids, args) = &rows[0];
        assert_eq!(*actor, Some(lucia_user));
        assert_eq!(ids, &vec![env.user("alex").to_string()]);
        assert!(args.starts_with("majority;role=guardian;categories="), "{args}");
    }
    Ok(())
}

#[tokio::test]
async fn an_adult_relative_claim_keeps_the_guardians() -> R {
    let env = Env::new(true).await?;
    let carmen = env.profile("carmen");
    exec(
        &env,
        &format!(
            "insert into profile_grant (user_id, profile_id, role, categories) \
             select '{}', $1, 'guardian', categories from profile_grant where profile_id = $1 limit 1",
            env.user("sam")
        ),
        carmen,
    )
    .await?;
    let mut tx = env.pool().begin().await?;
    let (user, _) = household::invite_member(
        &mut tx,
        env.fam().household_id,
        env.user("sam"),
        "Carmen Ortega",
        "es",
        Some(carmen),
    )
    .await?;
    household::claim_profile(&mut tx, carmen, user, Some(env.user("sam")), Local::now().date_naive()).await?;
    tx.commit().await?;
    assert_eq!(
        grants(&env, carmen).await?.get(&env.user("sam")).map(String::as_str),
        Some("guardian")
    );
    assert!(audits(&env, carmen).await?.is_empty());
    Ok(())
}

fn pending(household: &Value) -> Value {
    household["transfer_pending"].clone()
}

#[tokio::test]
async fn guardians_see_a_pending_transfer_from_the_birthday_on() -> R {
    let env = Env::new(true).await?;
    let lucia = env.profile("lucia");
    let today = Local::now().date_naive();
    let eighteen = today.checked_sub_months(Months::new(18 * 12)).unwrap_or(today);
    let mut alex = env.as_user(env.user("alex")).await?;
    let set_birth = |d: NaiveDate| format!("update profile set birth_date = '{d}' where id = $1");

    exec(&env, &set_birth(eighteen.succ_opt().unwrap_or(eighteen)), lucia).await?;
    assert_eq!(pending(&alex.get("/api/household").await?.json), json!([]));

    exec(&env, &set_birth(eighteen), lucia).await?;
    assert_eq!(pending(&alex.get("/api/household").await?.json), json!(["lucia"]));
    let mut sam = env.as_user(env.user("sam")).await?;
    assert_eq!(pending(&sam.get("/api/household").await?.json), json!(["lucia"]));

    // the household's own age decides: at 21 she is a minor again
    let r = alex
        .put("/api/household/settings", &json!({"age_of_majority": 21}))
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(pending(&alex.get("/api/household").await?.json), json!([]));
    Ok(())
}

#[tokio::test]
async fn only_the_household_owner_sets_the_age_of_majority() -> R {
    let env = Env::new(true).await?;
    let path = "/api/household/settings";
    let body = json!({"age_of_majority": 16});
    // Sam owns her own profile but did not set up the household
    let r = env.as_user(env.user("sam")).await?.put(path, &body).await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "forbidden"));
    let r = env.stale(env.user("alex")).await?.put(path, &body).await?;
    assert_eq!(r.code(), "step_up_required");
    let mut alex = env.as_user(env.user("alex")).await?;
    for bad in [
        json!({"age_of_majority": 13}),
        json!({"age_of_majority": 22}),
        json!({"age_of_majority": "18"}),
        json!({}),
    ] {
        let r = alex.put(path, &bad).await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad} {:?}", r.json);
    }
    assert_eq!(
        env.scalar_i64("select count(*) from household_setting where key = 'age_of_majority'")
            .await?,
        0
    );
    let r = alex.put(path, &body).await?;
    assert_eq!(
        (r.status, r.json.clone()),
        (StatusCode::OK, json!({"age_of_majority": 16}))
    );
    let stored: Value =
        sqlx::query_scalar("select value from household_setting where household_id = $1 and key = 'age_of_majority'")
            .bind(env.fam().household_id)
            .fetch_one(env.pool())
            .await?;
    assert_eq!(stored, json!(16));
    assert_eq!(
        env.scalar_i64(
            "select count(*) from audit_event where action = 'settings_change' \
             and args_summary = 'age_of_majority=16'"
        )
        .await?,
        1
    );
    // a 17-year-old is now an adult on every screen
    exec(
        &env,
        "update profile set birth_date = current_date - interval '17 years 30 days' where id = $1",
        env.profile("lucia"),
    )
    .await?;
    let me = alex.get("/api/me").await?.json;
    let lucia = me["profiles"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["slug"] == "lucia"))
        .cloned();
    assert_eq!(lucia.map(|p| p["is_minor"].clone()), Some(json!(false)));
    Ok(())
}
