//! `POST /api/profiles/{p}/summary.pdf`: step-up, read permission (a caregiver prints, a viewer gets
//! what its scopes allow), the sensitive genome gate (ticked, switched on, never for a minor), the
//! input checks and the audit row. People call over HTTP with real session cookies.

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_genome::synthetic;
use ownhealth_ingest_pdf::textcheck::assert_text_layer;
use serde_json::{Value, json};
use support::Env;

type R = anyhow::Result<()>;

/// The Rivera family with Alex's and Lucía's synthetic genome facts (APOE e3/e4 is sensitive).
async fn env() -> anyhow::Result<Env> {
    let env = Env::new(true).await?;
    let mut c = env.pool().acquire().await?;
    let raw = synthetic::alex_raw_files()?;
    let (facts, att) = synthetic::with_files(
        &synthetic::alex_facts(),
        &env.rt.blobs,
        ownhealth_core::blobs::BlobOwner::Reference,
        &raw,
    )?;
    for (slug, facts, att) in [("alex", facts, att), ("lucia", synthetic::lucia_facts(), Vec::new())] {
        synthetic::seed_genome(
            &mut c,
            &env.rt.blobs,
            env.fam().household_id,
            env.profile(slug),
            &facts,
            &att,
            "genome-facts.json",
            None,
        )
        .await?;
    }
    Ok(env)
}

fn lines(pdf: &[u8]) -> Vec<String> {
    assert!(pdf.starts_with(b"%PDF-"), "not a PDF");
    assert_text_layer(pdf, |_| Ok(()))
}

async fn sensitive_ui(env: &Env, slug: &str, on: bool) -> R {
    sqlx::query("update profile set genome_sensitive_ui = $2 where id = $1")
        .bind(env.profile(slug))
        .bind(on)
        .execute(env.pool())
        .await?;
    Ok(())
}

#[tokio::test]
async fn printing_needs_a_session_and_a_fresh_step_up() -> R {
    let env = env().await?;
    let body = json!({"lang": "en", "specialty": "gp"});
    let r = env.anon().post("/api/profiles/alex/summary.pdf", &body).await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{:?}", r.json);
    let r = env
        .stale(env.user("alex"))
        .await?
        .post("/api/profiles/alex/summary.pdf", &body)
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "step_up_required");
    let n = env
        .scalar_i64("select count(*) from audit_event where tool = 'POST /api/profiles/{p}/summary.pdf'")
        .await?;
    assert_eq!(n, 0, "a refused print writes no audit row");
    Ok(())
}

#[tokio::test]
async fn the_owner_prints_a_pdf_and_an_audit_row_is_written() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    for (lang, title) in [
        ("es", "Resumen para el médico · Cardiología"),
        ("en", "Doctor summary · Cardiology"),
    ] {
        let r = alex
            .post(
                "/api/profiles/alex/summary.pdf",
                &json!({"lang": lang, "specialty": "cardiology"}),
            )
            .await?;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        assert_eq!(r.header("content-type"), "application/pdf");
        assert_eq!(r.header("cache-control"), "no-store");
        assert!(
            r.header("content-disposition")
                .starts_with("inline; filename=\"ownhealth-summary-alex-cardiology-")
        );
        let text = lines(&r.bytes);
        assert_eq!(text[0], title);
        assert!(text.iter().any(|l| l.contains("CYP2C19")), "{text:#?}");
    }
    type Row = (
        String,
        String,
        Option<String>,
        Option<Vec<String>>,
        Option<String>,
        Option<Value>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select via, action, resource_type, src_ids, args_summary, withheld from audit_event \
         where tool = 'POST /api/profiles/{p}/summary.pdf' and profile_id = $1 order by id",
    )
    .bind(env.profile("alex"))
    .fetch_all(env.pool())
    .await?;
    assert_eq!(rows.len(), 2);
    let (via, action, kind, srcs, args, withheld) = &rows[0];
    assert_eq!((via.as_str(), action.as_str()), ("export", "export"));
    assert_eq!(kind.as_deref(), Some("summary"));
    assert!(
        srcs.as_ref().is_some_and(|s| !s.is_empty()),
        "the printed src ids are named"
    );
    assert!(
        args.as_deref()
            .is_some_and(|a| a.starts_with("specialty=cardiology;lang=es;sensitive=0")),
        "{args:?}"
    );
    assert_eq!(withheld.as_ref().map(|w| w["by_scope"].clone()), Some(json!([])));
    Ok(())
}

#[tokio::test]
async fn a_caregiver_prints_and_a_viewer_gets_labs_records_and_genome_withheld() -> R {
    let env = env().await?;
    // Alex is Carmen's caregiver
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex
        .post(
            "/api/profiles/carmen/summary.pdf",
            &json!({"lang": "es", "specialty": "gp"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // Sam is a viewer of Alex: read:summary only
    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam
        .post(
            "/api/profiles/alex/summary.pdf",
            &json!({"lang": "en", "specialty": "gp"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let text = lines(&r.bytes);
    assert!(
        text.iter()
            .any(|l| l.starts_with("Lab values and calculated values: not shown")),
        "{text:#?}"
    );
    assert!(
        text.iter().any(|l| l.starts_with("Genome findings: not shown")),
        "{text:#?}"
    );
    assert!(
        !text.iter().any(|l| l.contains("CYP2C19") || l.contains("Lab range")),
        "{text:#?}"
    );
    let w: Value = sqlx::query_scalar(
        "select withheld from audit_event where tool = 'POST /api/profiles/{p}/summary.pdf' and actor_id = $1",
    )
    .bind(env.user("sam"))
    .fetch_one(env.pool())
    .await?;
    assert_eq!(w["by_scope"], json!(["labs", "records", "genome"]));
    Ok(())
}

#[tokio::test]
async fn a_sensitive_finding_prints_only_when_ticked_and_never_for_a_minor() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let mut apoe = async |slug: &str, tick: bool| -> anyhow::Result<bool> {
        let body = json!({"lang": "en", "specialty": "gp", "include_sensitive": tick});
        let r = alex.post(&format!("/api/profiles/{slug}/summary.pdf"), &body).await?;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        let text = lines(&r.bytes);
        // the sheet goes to a third party: it never says that a gated finding exists, nor how many
        assert!(!text.iter().any(|l| l.contains("not printed")), "{text:#?}");
        Ok(text.iter().any(|l| l.contains("APOE")))
    };
    // the profile's UI switch is off: ticking alone does not open it
    sensitive_ui(&env, "alex", false).await?;
    assert!(!apoe("alex", true).await?);
    sensitive_ui(&env, "alex", true).await?;
    assert!(!apoe("alex", false).await?, "absent unless ticked");
    assert!(apoe("alex", true).await?, "ticked and allowed");
    // Lucía is 9: never, whatever the switch and the tick say
    sensitive_ui(&env, "lucia", true).await?;
    assert!(!apoe("lucia", true).await?);
    // nor by asking for a date on which she is an adult: a print is never dated after today
    let body = json!({"lang": "en", "specialty": "gp", "include_sensitive": true, "as_of": "2045-01-01"});
    let r = alex.post("/api/profiles/lucia/summary.pdf", &body).await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", r.json);
    Ok(())
}

#[tokio::test]
async fn an_unknown_specialty_or_language_is_refused() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    for body in [
        json!({"lang": "en", "specialty": "dermatology"}),
        json!({"lang": "fr", "specialty": "gp"}),
    ] {
        let r = alex.post("/api/profiles/alex/summary.pdf", &body).await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    Ok(())
}

/// Wait until a statement of another session waits for a lock on `table`.
async fn wait_for_a_lock_on(pool: &sqlx::PgPool, table: &str) -> R {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let n: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity where datname = current_database() \
             and pid <> pg_backend_pid() and wait_event_type = 'Lock' and query ilike '%' || $1 || '%'",
        )
        .bind(table)
        .fetch_one(pool)
        .await?;
        if n > 0 {
            return Ok(());
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no statement waited for {table}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

const VACCINE: &str = "Tetanus booster 7f3a";
const PROCEDURE: &str = "Knee arthroscopy 7f3a";
/// A condition is part of the hashed state, so the correction moves the state hash.
const CONDITION: &str = "Asthma 7f3a";
const PRINT: &str = "POST /api/profiles/{p}/summary.pdf";
const IPS: &str = "POST /api/profiles/export";

/// What else happens while the request waits.
#[derive(Clone, Copy, PartialEq)]
enum Meanwhile {
    Nothing,
    /// A reader saves the corrected state over the day's older row, as a dashboard view does.
    StoreOverAnOlderRow,
    /// The same, when the day has no row yet.
    StoreTheFirstRow,
}

/// What a request asked during a correction left behind.
struct Race {
    bytes: Vec<u8>,
    /// The state hash and time of the request's audit row.
    hash: String,
    at: DateTime<Utc>,
    /// The time of the correction's audit row.
    corrected_at: DateTime<Utc>,
    /// The hash the reader stored during the wait.
    newer: Option<String>,
}

fn hex(hash: &str) -> &str {
    hash.split_once(':').map_or(hash, |(_, x)| x)
}

/// The hash of the profile's newest stored state.
async fn stored(env: &Env, slug: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "select encode(state_hash, 'hex') from state_snapshot where profile_id = $1 order by as_of desc limit 1",
    )
    .bind(env.profile(slug))
    .fetch_optional(env.pool())
    .await?)
}

/// The state hash and time of the newest audit row of `tool`.
async fn last_audit(env: &Env, tool: &str) -> anyhow::Result<(String, DateTime<Utc>)> {
    let (ids, at): (Vec<String>, DateTime<Utc>) =
        sqlx::query_as("select resource_ids, ts from audit_event where tool = $1 order by id desc limit 1")
            .bind(tool)
            .fetch_one(env.pool())
            .await?;
    Ok((ids[0].clone(), at))
}

/// Alex asks for `path` while a correction is saved. The request reads the vaccines, then waits to
/// read the procedures, because another writer holds that table. Meanwhile a vaccine and a
/// condition are saved with their audit row, then the procedure lands and the lock goes.
async fn ask_during_a_correction(
    env: &Env,
    alex: &mut support::Client,
    (path, tool): (&str, &str),
    body: &Value,
    meanwhile: Meanwhile,
) -> anyhow::Result<Race> {
    let (pool, pid, by) = (env.pool().clone(), env.profile("alex"), env.user("alex"));
    let today = ownhealth_core::access::today_utc();
    match meanwhile {
        Meanwhile::Nothing => {}
        Meanwhile::StoreOverAnOlderRow => {
            ownhealth_rules::build_state(&mut *pool.acquire().await?, pid, today, None).await?;
            anyhow::ensure!(stored(env, "alex").await?.is_some(), "the day has a row");
        }
        Meanwhile::StoreTheFirstRow => {
            sqlx::query("delete from state_snapshot where profile_id = $1")
                .bind(pid)
                .execute(&pool)
                .await?;
        }
    }
    let src = SrcRef::New(SrcIn {
        profile_id: Some(pid),
        ..SrcIn::manual(Some(by))
    });
    let add = async |conn: &mut sqlx::PgConnection, kind: &str, row: Value| -> R {
        add_record(
            conn,
            &RecordIn::from_json(kind, row)?,
            &src,
            RecordOptions::new("person"),
        )
        .await?;
        Ok(())
    };
    let mut lock = pool.begin().await?;
    sqlx::query("lock table procedure in access exclusive mode")
        .execute(&mut *lock)
        .await?;
    let correct = async {
        wait_for_a_lock_on(&pool, "procedure").await?;
        let mut c = pool.begin().await?;
        add(
            &mut c,
            "immunization",
            json!({"profile_id": pid, "vaccine": VACCINE, "occurred": "2026-11-02"}),
        )
        .await?;
        let condition =
            json!({"profile_id": pid, "text": CONDITION, "onset": "2026-11-02", "verification_status": "confirmed"});
        add(&mut c, "condition", condition).await?;
        let id = write_audit(
            &mut c,
            &AuditEvent {
                actor_id: Some(by),
                profile_id: Some(pid),
                tool: Some("correction".into()),
                ..AuditEvent::new("user", "ui", "write")
            },
        )
        .await?;
        let corrected_at: DateTime<Utc> = sqlx::query_scalar("select ts from audit_event where id = $1")
            .bind(id)
            .fetch_one(&mut *c)
            .await?;
        c.commit().await?;
        let newer = match meanwhile {
            Meanwhile::Nothing => None,
            _ => {
                let state = ownhealth_rules::build_state(&mut *pool.acquire().await?, pid, today, None).await?;
                Some(state["state_hash"].as_str().unwrap_or_default().to_string())
            }
        };
        add(
            &mut lock,
            "procedure",
            json!({"profile_id": pid, "text": PROCEDURE, "performed": "2026-11-02"}),
        )
        .await?;
        lock.commit().await?;
        anyhow::Ok((corrected_at, newer))
    };
    let (asked, saved) = tokio::join!(alex.post(path, body), correct);
    let (corrected_at, newer) = saved?;
    let asked = asked?;
    assert_eq!(asked.status, StatusCode::OK, "{:?}", asked.json);
    let (hash, at) = last_audit(env, tool).await?;
    Ok(Race {
        bytes: asked.bytes,
        hash,
        at,
        corrected_at,
        newer,
    })
}

/// A correction saved while a sheet prints lands on all of the sheet or on none of it. The sheet
/// holds the state from when the print began, and its audit row says so: it is older than the
/// correction's row. That state is the one stored, and the next print shows the correction.
#[tokio::test]
async fn a_correction_saved_during_a_print_never_mixes_two_states_on_one_sheet() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (path, body) = (
        "/api/profiles/alex/summary.pdf",
        json!({"lang": "en", "specialty": "gp"}),
    );
    let shows = |text: &[String], what: &str| text.iter().any(|l| l.contains(what));
    let race = ask_during_a_correction(&env, &mut alex, (path, PRINT), &body, Meanwhile::Nothing).await?;
    let text = lines(&race.bytes);
    let parts = [VACCINE, CONDITION, PROCEDURE].map(|w| shows(&text, w));
    assert_eq!(
        parts, [false; 3],
        "one state on the sheet, from when the print began: {text:#?}"
    );
    assert!(
        race.at < race.corrected_at,
        "the print is logged before the correction it does not show"
    );
    assert_eq!(
        stored(&env, "alex").await?.as_deref(),
        Some(hex(&race.hash)),
        "the state read is the state stored"
    );
    let text = lines(&alex.post(path, &body).await?.bytes);
    assert_eq!(
        [VACCINE, CONDITION, PROCEDURE].map(|w| shows(&text, w)),
        [true; 3],
        "the next print shows the correction: {text:#?}"
    );
    assert_ne!(
        last_audit(&env, PRINT).await?.0,
        race.hash,
        "the correction moved the state"
    );
    Ok(())
}

/// The IPS export is built from the same summary, so it holds one state too.
#[tokio::test]
async fn a_correction_saved_during_an_ips_export_never_mixes_two_states() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (path, body) = ("/api/profiles/alex/export", json!({"fmt": "ips", "lang": "en"}));
    let has = |bytes: &[u8], what: &str| String::from_utf8_lossy(bytes).contains(what);
    let race = ask_during_a_correction(&env, &mut alex, (path, IPS), &body, Meanwhile::Nothing).await?;
    let parts = [VACCINE, CONDITION, PROCEDURE].map(|w| has(&race.bytes, w));
    assert_eq!(parts, [false; 3], "one state in the Bundle, from when the export began");
    assert!(
        race.at < race.corrected_at,
        "the export is logged before the correction it does not show"
    );
    assert_eq!(
        stored(&env, "alex").await?.as_deref(),
        Some(hex(&race.hash)),
        "the state read is the state stored"
    );
    let bundle = alex.post(path, &body).await?.bytes;
    assert_eq!(
        [VACCINE, CONDITION, PROCEDURE].map(|w| has(&bundle, w)),
        [true; 3],
        "the next export shows the correction"
    );
    assert_ne!(
        last_audit(&env, IPS).await?.0,
        race.hash,
        "the correction moved the state"
    );
    Ok(())
}

/// A print that read an older state never stores it over a newer one saved while it waited. Its
/// write is dropped, and it still answers with the sheet it read and its audit row.
async fn a_newer_state_is_kept(path: &str, tool: &str, body: Value, meanwhile: Meanwhile) -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let race = ask_during_a_correction(&env, &mut alex, (path, tool), &body, meanwhile).await?;
    let newer = race.newer.unwrap_or_default();
    assert_ne!(newer, race.hash, "the reader saved the corrected state");
    assert_eq!(
        stored(&env, "alex").await?.as_deref(),
        Some(hex(&newer)),
        "the newer state stays stored"
    );
    let text = if race.bytes.starts_with(b"%PDF-") {
        let text = lines(&race.bytes);
        let own: String = hex(&race.hash).chars().take(12).collect();
        assert!(
            text.iter().any(|l| l.contains(&own)),
            "the sheet carries the state it read: {text:#?}"
        );
        text.join("\n")
    } else {
        String::from_utf8_lossy(&race.bytes).into_owned()
    };
    assert_eq!(
        [VACCINE, CONDITION, PROCEDURE].map(|w| text.contains(w)),
        [false; 3],
        "the answer is the state it read, not the newer one"
    );
    let rows: i64 = sqlx::query_scalar("select count(*) from audit_event where tool = $1 and resource_ids[1] = $2")
        .bind(tool)
        .bind(&race.hash)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(rows, 1, "one audit row, for the state it sent");
    Ok(())
}

#[tokio::test]
async fn a_newer_state_saved_during_a_print_is_kept() -> R {
    let body = json!({"lang": "en", "specialty": "gp"});
    a_newer_state_is_kept(
        "/api/profiles/alex/summary.pdf",
        PRINT,
        body,
        Meanwhile::StoreOverAnOlderRow,
    )
    .await
}

#[tokio::test]
async fn a_first_state_of_the_day_saved_during_a_print_is_kept() -> R {
    let body = json!({"lang": "en", "specialty": "gp"});
    a_newer_state_is_kept(
        "/api/profiles/alex/summary.pdf",
        PRINT,
        body,
        Meanwhile::StoreTheFirstRow,
    )
    .await
}

#[tokio::test]
async fn a_newer_state_saved_during_an_ips_export_is_kept() -> R {
    let body = json!({"fmt": "ips", "lang": "en"});
    a_newer_state_is_kept("/api/profiles/alex/export", IPS, body, Meanwhile::StoreOverAnOlderRow).await
}

/// Only a newer stored state lets a print drop its write: any other failure of the store fails the
/// request, and nothing is sent or logged.
#[tokio::test]
async fn a_failed_state_store_fails_the_request() -> R {
    let env = env().await?;
    sqlx::raw_sql(
        "create function no_snapshot() returns trigger language plpgsql as $$ begin \
           raise exception 'no room for a snapshot' using errcode = '53100'; end $$; \
         create trigger no_snapshot before insert or update on state_snapshot \
           for each row execute function no_snapshot();",
    )
    .execute(env.pool())
    .await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    for (path, tool, body) in [
        (
            "/api/profiles/alex/summary.pdf",
            PRINT,
            json!({"lang": "en", "specialty": "gp"}),
        ),
        ("/api/profiles/alex/export", IPS, json!({"fmt": "ips", "lang": "en"})),
    ] {
        sqlx::query("delete from state_snapshot where profile_id = $1")
            .bind(env.profile("alex"))
            .execute(env.pool())
            .await?;
        let r = alex.post(path, &body).await?;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{tool}: {:?}", r.json);
        assert!(
            !r.bytes.starts_with(b"%PDF-") && !String::from_utf8_lossy(&r.bytes).contains("\"Bundle\""),
            "{tool}: nothing is sent"
        );
        let rows: i64 = sqlx::query_scalar("select count(*) from audit_event where tool = $1")
            .bind(tool)
            .fetch_one(env.pool())
            .await?;
        assert_eq!(rows, 0, "{tool}: nothing is logged");
    }
    Ok(())
}

/// The sheet is read from one snapshot: its transaction is REPEATABLE READ.
#[tokio::test]
async fn the_sheet_is_read_in_a_repeatable_read_transaction() -> R {
    let env = Env::new(true).await?;
    let mut tx = ownhealth_rules::summary::begin_read(env.pool()).await?;
    let isolation: String = sqlx::query_scalar("select current_setting('transaction_isolation')")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(isolation, "repeatable read");
    Ok(())
}

/// The audit row is stamped with the moment the read began, not with the moment it is written:
/// here the read waits 1.5 s on a lock before its audit row is written.
#[tokio::test]
async fn the_audit_row_carries_the_time_the_read_began() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let pool = env.pool().clone();
    for (path, tool, body) in [
        (
            "/api/profiles/alex/summary.pdf",
            PRINT,
            json!({"lang": "es", "specialty": "gp"}),
        ),
        ("/api/profiles/alex/export", IPS, json!({"fmt": "ips", "lang": "es"})),
    ] {
        let clock = || sqlx::query_scalar::<_, DateTime<Utc>>("select clock_timestamp()").fetch_one(&pool);
        let sent_at = clock().await?;
        let mut lock = pool.begin().await?;
        sqlx::query("lock table procedure in access exclusive mode")
            .execute(&mut *lock)
            .await?;
        let hold = async {
            wait_for_a_lock_on(&pool, "procedure").await?;
            let waiting_at = clock().await?;
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            lock.rollback().await?;
            anyhow::Ok(waiting_at)
        };
        let (asked, held) = tokio::join!(alex.post(path, &body), hold);
        let waiting_at = held?;
        assert_eq!(asked?.status, StatusCode::OK, "{tool}");
        let (_, at) = last_audit(&env, tool).await?;
        assert!(
            sent_at <= at && at < waiting_at,
            "{tool}: stamped at the read, {sent_at} <= {at} < {waiting_at}"
        );
    }
    Ok(())
}

/// The audit chain (slice 12) links a row only from a READ COMMITTED transaction. A trigger with
/// that rule stands in for it here: the sheet and the Bundle still go out, each with its row.
#[tokio::test]
async fn the_audit_row_is_written_in_a_read_committed_transaction() -> R {
    let env = env().await?;
    sqlx::raw_sql(
        "create function read_committed_only() returns trigger language plpgsql as $$ begin \
           if current_setting('transaction_isolation') <> 'read committed' then \
             raise exception 'audit_event is written only in read committed transactions'; \
           end if; return new; end $$; \
         create trigger read_committed_only before insert on audit_event \
           for each row execute function read_committed_only();",
    )
    .execute(env.pool())
    .await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    for (path, tool, body) in [
        (
            "/api/profiles/alex/summary.pdf",
            PRINT,
            json!({"lang": "en", "specialty": "gp"}),
        ),
        ("/api/profiles/alex/export", IPS, json!({"fmt": "ips", "lang": "en"})),
    ] {
        let r = alex.post(path, &body).await?;
        assert_eq!(r.status, StatusCode::OK, "{tool}: {:?}", r.json);
        last_audit(&env, tool).await?;
    }
    Ok(())
}

/// A stored locale the registry lacks (an older install, or a rollback after a language PR) prints
/// in the default language: only a `lang` the caller sent can be refused.
#[tokio::test]
async fn a_stored_locale_outside_the_registry_prints_in_the_default() -> R {
    let env = env().await?;
    sqlx::query("update app_user set locale = 'de' where id = $1")
        .bind(env.user("alex"))
        .execute(env.pool())
        .await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex
        .post("/api/profiles/alex/summary.pdf", &json!({"specialty": "gp"}))
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = alex
        .post(
            "/api/profiles/alex/summary.pdf",
            &json!({"lang": "de", "specialty": "gp"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json["detail"][0]["loc"], json!(["body", "lang"]), "{:?}", r.json);
    // the IPS export reads the same way
    let r = alex.post("/api/profiles/alex/export", &json!({"fmt": "ips"})).await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = alex
        .post("/api/profiles/alex/export", &json!({"fmt": "ips", "lang": "de"}))
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}
