//! The deploy files (deploy/, compose.yaml), checked from outside the app.
//!
//! - The app's database role cannot rewrite the audit log (security-privacy.md section 9).
//!   deploy/db/app-role.sql runs as the table owner after the migrations. Connected as the role it
//!   creates, INSERT and SELECT on audit_event work, and UPDATE, DELETE, TRUNCATE and ALTER TABLE ...
//!   DISABLE TRIGGER are refused by privileges, not only by the trigger. The owner control shows the
//!   same ALTER succeeds for the owner, so the test can fail. Needs psql 15 or newer on PATH, or the
//!   test container (scripts/test-db.sh).
//! - What the two compose files hand the app container, rendered by `docker compose config`: the app
//!   binds 0.0.0.0, connects as the non-owner role and does not migrate; the admin service migrates as
//!   the owner; the homelab app gets no backup credentials. Skipped without docker compose.
//! - The app-role.sql leaves no door open to other roles: PUBLIC loses CONNECT on the database.
//! - The `wearables` profile (the Open Wearables sidecar): pinned by digest in both compose files,
//!   telemetry and Sentry off, no published port, none of OwnHealth's secrets, Redis on an internal
//!   network, and a database of its own. `wearables_profile_comes_up_as_the_restricted_role` brings
//!   the profile up for real (CI, or OWNHEALTH_TEST_WEARABLES=1): OW's migrations run on Postgres 17
//!   as a role that is no superuser and cannot connect to the health database.
//! - The public site block of deploy/Caddyfile, run in the caddy image compose.homelab.yaml pins, in
//!   front of a stub upstream: it passes only the SMART Health Link routes (the manifest POST and
//!   its preflight, the file GET), always in their canonical form whatever raw path the client
//!   wrote, and answers 404 to everything else, /login, /api, /mcp and /dicom-web included. It keeps
//!   the X-Forwarded-For of the tunnel in front of it. Skipped without docker; the image is pulled
//!   when missing.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ownhealth_core::config::Settings;
use ownhealth_core::testing::{TestDb, admin_url};
use serde_json::Value;
use sqlx::{Connection, PgConnection};

type R = anyhow::Result<()>;

const CONTAINER: &str = "ownhealth-testpg";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn ok(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// psql 15 or newer on PATH.
fn host_psql() -> bool {
    Command::new("psql")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.split_whitespace()
                .find_map(|w| w.split('.').next()?.parse::<u32>().ok())
        })
        .is_some_and(|major| major >= 15)
}

/// `postgres://user:password@host:port/<db>` from the admin URL, with another login when given.
fn url(db: &str, login: Option<(&str, &str)>) -> String {
    let admin = admin_url();
    let base = admin.rsplit_once('/').map_or(admin.as_str(), |(b, _)| b);
    let base = match (login, base.split_once("://"), base.rsplit_once('@')) {
        (Some((user, password)), Some((scheme, _)), Some((_, host))) => format!("{scheme}://{user}:{password}@{host}"),
        _ => base.to_string(),
    };
    format!("{base}/{db}")
}

fn apply_role_sql(db: &str, role: &str, password: &str) -> anyhow::Result<Output> {
    let sql = std::fs::read(root().join("deploy/db/app-role.sql"))?;
    let args = [
        "-X",
        "-q",
        "-v",
        "ON_ERROR_STOP=1",
        "-v",
        &format!("app_role={role}"),
        "-f",
        "-",
    ];
    let mut cmd = if host_psql() {
        let mut c = Command::new("psql");
        c.arg(url(db, None)).args(args);
        c
    } else {
        let mut c = Command::new("docker");
        c.args([
            "exec",
            "-i",
            "-e",
            "OWNHEALTH_APP_ROLE_PASSWORD",
            CONTAINER,
            "psql",
            "-U",
            "postgres",
            "-d",
            db,
        ])
        .args(args);
        c
    };
    let mut child = cmd
        .env("OWNHEALTH_APP_ROLE_PASSWORD", password)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?
        .write_all(&sql)?;
    Ok(child.wait_with_output()?)
}

async fn refused(conn: &mut PgConnection, sql: &str) -> anyhow::Result<String> {
    match sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&mut *conn)
        .await
    {
        Ok(_) => anyhow::bail!("{sql}: allowed"),
        Err(e) => Ok(e.to_string()),
    }
}

async fn check_app_role(db: &TestDb, role: &str, password: &str) -> R {
    for _ in 0..2 {
        // idempotent: the admin service runs it after every migration
        let done = apply_role_sql(&db.name, role, password)?;
        anyhow::ensure!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    }
    let mut app = PgConnection::connect(&url(&db.name, Some((role, password)))).await?;
    sqlx::query("insert into audit_event (actor_kind, via, action) values ('system', 'job', 'test')")
        .execute(&mut app)
        .await?;
    let n: i64 = sqlx::query_scalar("select count(*) from audit_event")
        .fetch_one(&mut app)
        .await?;
    anyhow::ensure!(n >= 1, "the app role reads no audit row");
    // the row still gets its chain link, which only the owner's trigger function writes
    let linked: i64 = sqlx::query_scalar(
        "select count(*) from audit_chain c join audit_event e on e.id = c.event_id where e.action = 'test'",
    )
    .fetch_one(&mut app)
    .await?;
    anyhow::ensure!(linked == 1, "chain links of the app role's row: {linked}");
    sqlx::query("select count(*) from profile").execute(&mut app).await?; // ordinary tables stay usable
    for sql in [
        "update audit_event set action = 'rewritten'",
        "delete from audit_event",
        "truncate audit_event",
        "insert into audit_chain (seq, event_id, prev_hash, hash) values (999999, 999999, '\\x00', '\\x00')",
        "update audit_chain set hash = prev_hash",
        "delete from audit_chain",
        "truncate audit_chain",
    ] {
        let e = refused(&mut app, sql).await?;
        anyhow::ensure!(e.contains("permission denied"), "{sql}: {e}");
    }
    for sql in [
        "alter table audit_event disable trigger all",
        "drop trigger audit_event_no_update on audit_event",
    ] {
        let e = refused(&mut app, sql).await?;
        anyhow::ensure!(e.contains("must be owner"), "{sql}: {e}");
    }
    app.close().await?;

    // owner control: the same ALTER succeeds for the table owner
    let mut owner = db.pool.begin().await?;
    sqlx::query("alter table audit_event disable trigger all")
        .execute(&mut *owner)
        .await?;
    sqlx::query("update audit_event set action = 'rewritten'")
        .execute(&mut *owner)
        .await?;
    owner.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn app_role_can_append_and_read_but_not_rewrite_the_audit_log() -> R {
    let has_docker = ok(Command::new("docker").args(["exec", CONTAINER, "true"]));
    if !host_psql() && !has_docker {
        eprintln!("no psql 15+ and no test container: skipped");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let role = format!("oh_test_app_{}", ownhealth_core::ids::token_hex(4));
    let password = ownhealth_core::ids::token_hex(16);
    let result = check_app_role(&db, &role, &password).await;
    let name = db.name.clone();
    drop(db); // drops the database, so the role owns nothing any more
    let mut admin = PgConnection::connect(&admin_url()).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!("drop role if exists \"{role}\"")))
        .execute(&mut admin)
        .await?;
    let left: i64 = sqlx::query_scalar("select count(*) from pg_database where datname = $1")
        .bind(&name)
        .fetch_one(&mut admin)
        .await?;
    assert_eq!(left, 0);
    result
}

/// Drop the test database, then the role, which then owns nothing.
async fn drop_db_and_role(db: TestDb, role: &str) -> R {
    let name = db.name.clone();
    drop(db);
    let mut admin = PgConnection::connect(&admin_url()).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!("drop role if exists \"{role}\"")))
        .execute(&mut admin)
        .await?;
    let left: i64 = sqlx::query_scalar("select count(*) from pg_database where datname = $1")
        .bind(&name)
        .fetch_one(&mut admin)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}

/// The app role, with the grants app-role.sql gives, tries to run its own code as the owner of the
/// SECURITY DEFINER chain function: it attaches the function to a temp table of its own and gives
/// that table an implicit cast to audit_event, which the function would call. The owner is a
/// superuser in both compose files.
async fn check_no_code_as_the_owner(db: &TestDb, role: &str, password: &str) -> R {
    let done = apply_role_sql(&db.name, role, password)?;
    anyhow::ensure!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    let mut app = PgConnection::connect(&url(&db.name, Some((role, password)))).await?;
    sqlx::raw_sql("create temp table oh_log (who text, su boolean); grant all on oh_log to public")
        .execute(&mut app)
        .await?;
    let attack = sqlx::raw_sql(
        "create temp table t (like public.audit_event); \
         create function pg_temp.f(r t) returns public.audit_event language plpgsql as $f$ \
           declare e public.audit_event; begin \
             insert into pg_temp.oh_log select current_user, rolsuper from pg_catalog.pg_roles \
              where rolname = current_user; \
             select * into e from public.audit_event order by id limit 1; return e; end $f$; \
         create cast (t as public.audit_event) with function pg_temp.f(t) as implicit; \
         create trigger x after insert on t for each row execute function public.oh_audit_chain_link(); \
         insert into t (id, ts, actor_kind, via, action) values (424242, pg_catalog.now(), 'user', 'ui', 'read');",
    )
    .execute(&mut app)
    .await;
    let ran_as: Vec<(String, bool)> = sqlx::query_as("select who, su from pg_temp.oh_log")
        .fetch_all(&mut app)
        .await?;
    app.close().await?;
    let forged: i64 = sqlx::query_scalar("select count(*) from audit_chain where event_id = 424242")
        .fetch_one(&db.pool)
        .await?;
    // checks that return their failure, so the caller still drops the role
    anyhow::ensure!(ran_as.is_empty(), "the app role's code ran as {ran_as:?}");
    anyhow::ensure!(forged == 0, "the app role wrote a chain link");
    let Err(e) = attack else {
        anyhow::bail!("the app role must not attach the chain function to its own table");
    };
    anyhow::ensure!(
        e.to_string()
            .contains("permission denied for function public.oh_audit_chain_link"),
        "{e}"
    );
    Ok(())
}

#[tokio::test]
async fn app_role_cannot_run_code_as_the_owner() -> R {
    let has_docker = ok(Command::new("docker").args(["exec", CONTAINER, "true"]));
    if !host_psql() && !has_docker {
        eprintln!("no psql 15+ and no test container: skipped");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let role = format!("oh_test_app_{}", ownhealth_core::ids::token_hex(4));
    let password = ownhealth_core::ids::token_hex(16);
    let result = check_no_code_as_the_owner(&db, &role, &password).await;
    drop_db_and_role(db, &role).await?;
    result
}

/// app-role.sh applies app-role.sql as one transaction, so a failed step leaves nothing behind.
/// Run statement by statement, the blanket grant commits before the revoke on audit_chain, and the
/// app role can write chain links in between. A REVOKE refused by an event trigger stands in for
/// any failure after that grant.
#[tokio::test]
async fn app_role_script_keeps_nothing_when_a_step_fails() -> R {
    if !host_psql() {
        eprintln!("no psql 15+ on PATH: skipped");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let role = format!("oh_test_app_{}", ownhealth_core::ids::token_hex(4));
    let password = ownhealth_core::ids::token_hex(16);
    let result: R = async {
        let mut owner = PgConnection::connect(&db.url).await?;
        sqlx::raw_sql(
            "create function oh_no_revoke() returns event_trigger language plpgsql as $f$ \
               begin raise exception 'oh_test: revoke refused'; end $f$; \
             create event trigger oh_no_revoke on ddl_command_start when tag in ('REVOKE') \
               execute function oh_no_revoke();",
        )
        .execute(&mut owner)
        .await?;
        owner.close().await?;
        let run = || {
            Command::new("sh")
                .arg(root().join("deploy/db/app-role.sh"))
                .env("OWNHEALTH_DATABASE_URL", url(&db.name, None))
                .env("OWNHEALTH_DB_PASSWORD", "")
                .env_remove("OWNHEALTH_DB_PASSWORD_FILE")
                .env("OWNHEALTH_APP_DB_PASSWORD", &password)
                .env_remove("OWNHEALTH_APP_DB_PASSWORD_FILE")
                .env("OWNHEALTH_APP_DB_ROLE", &role)
                .output()
        };
        let out = run()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::ensure!(!out.status.success(), "app-role.sh must fail: {stderr}");
        anyhow::ensure!(stderr.contains("revoke refused"), "{stderr}");
        let mut admin = PgConnection::connect(&admin_url()).await?;
        let left: i64 = sqlx::query_scalar("select count(*) from pg_roles where rolname = $1")
            .bind(&role)
            .fetch_one(&mut admin)
            .await?;
        admin.close().await?;
        anyhow::ensure!(left == 0, "the failed run left the role {role} behind");
        // without the failure, the same single transaction runs through and gives the role its rights
        let mut owner = PgConnection::connect(&db.url).await?;
        sqlx::raw_sql("drop event trigger oh_no_revoke")
            .execute(&mut owner)
            .await?;
        let out = run()?;
        anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let rights: (bool, bool) = sqlx::query_as(
            "select has_table_privilege($1, 'audit_event', 'insert'), has_table_privilege($1, 'audit_chain', 'insert')",
        )
        .bind(&role)
        .fetch_one(&mut owner)
        .await?;
        owner.close().await?;
        anyhow::ensure!(
            rights == (true, false),
            "insert on audit_event, audit_chain: {rights:?}"
        );
        Ok(())
    }
    .await;
    drop_db_and_role(db, &role).await?;
    result
}

/// app-role.sql closes the database to every role it did not grant, such as the Open Wearables role
/// on the same server: PUBLIC loses CONNECT. The control connects as that fresh role before the
/// script runs, so the check can fail.
#[tokio::test]
async fn app_role_sql_closes_the_database_to_other_roles() -> R {
    let has_docker = ok(Command::new("docker").args(["exec", CONTAINER, "true"]));
    if !host_psql() && !has_docker {
        eprintln!("no psql 15+ and no test container: skipped");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let tag = ownhealth_core::ids::token_hex(4);
    let (app, other) = (format!("oh_test_app_{tag}"), format!("oh_test_other_{tag}"));
    let password = ownhealth_core::ids::token_hex(16);
    let mut admin = PgConnection::connect(&admin_url()).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "create role \"{other}\" login password '{password}'"
    )))
    .execute(&mut admin)
    .await?;
    let result = async {
        let other_url = url(&db.name, Some((&other, &password)));
        PgConnection::connect(&other_url)
            .await
            .map_err(|e| anyhow::anyhow!("control: a fresh role could not connect before app-role.sql: {e}"))?
            .close()
            .await?;
        let done = apply_role_sql(&db.name, &app, &password)?;
        anyhow::ensure!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
        let e = match PgConnection::connect(&other_url).await {
            Ok(_) => anyhow::bail!("{other} still connects after app-role.sql"),
            Err(e) => e.to_string(),
        };
        anyhow::ensure!(e.contains("permission denied for database"), "{e}");
        // the app role keeps its own grant
        PgConnection::connect(&url(&db.name, Some((&app, &password))))
            .await?
            .close()
            .await?;
        Ok(())
    }
    .await;
    drop(db);
    for role in [&app, &other] {
        sqlx::query(sqlx::AssertSqlSafe(format!("drop role if exists \"{role}\"")))
            .execute(&mut admin)
            .await?;
    }
    result
}

// ------------------------------------------------------------------ compose ----

fn has_compose() -> bool {
    ok(Command::new("docker").args(["compose", "version"]))
}

/// The services of `docker compose config --format json` (see `render_doc`).
fn render(compose: &Path, env_file: &Path, extra: &str, files: &[(&str, String)]) -> anyhow::Result<Value> {
    Ok(render_doc(compose, env_file, extra, files)?["services"].clone())
}

/// `docker compose config --format json` of `compose` with `env_file` (+ `extra`) as its .env.
fn render_doc(compose: &Path, env_file: &Path, extra: &str, files: &[(&str, String)]) -> anyhow::Result<Value> {
    let dir = tempfile::tempdir()?;
    std::fs::copy(compose, dir.path().join("compose.yaml"))?;
    std::fs::write(dir.path().join(".env"), std::fs::read_to_string(env_file)? + extra)?;
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body)?;
    }
    render_config(dir.path(), None)
}

fn render_config(dir: &Path, project: Option<&str>) -> anyhow::Result<Value> {
    let mut cmd = Command::new("docker");
    cmd.arg("compose");
    if let Some(project) = project {
        cmd.args(["-p", project]);
    }
    let out = cmd
        .args(["config", "--format", "json"])
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
        .output()?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn cases() -> [(&'static str, PathBuf, PathBuf, &'static str); 2] {
    let r = root();
    [
        (
            "homelab",
            r.join("deploy/compose.homelab.yaml"),
            r.join("deploy/env.homelab.example"),
            "",
        ),
        (
            "quickstart",
            r.join("compose.yaml"),
            r.join(".env.example"),
            "OWNHEALTH_DB_PASSWORD=x\nOWNHEALTH_APP_DB_PASSWORD=y\n",
        ),
    ]
}

fn env_str(v: &Value, key: &str) -> String {
    v["environment"][key].as_str().unwrap_or_default().to_string()
}

#[test]
fn two_compose_projects_in_one_checkout_use_distinct_keyring_folders() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    for (case, compose, env_file, extra) in cases() {
        let dir = tempfile::tempdir()?;
        std::fs::copy(compose, dir.path().join("compose.yaml"))?;
        std::fs::write(dir.path().join(".env"), std::fs::read_to_string(env_file)? + extra)?;
        let mut sources = Vec::new();
        for project in ["ownhealth", "ownhealth-demo"] {
            let config = render_config(dir.path(), Some(project))?;
            let source = |name: &str| -> anyhow::Result<&str> {
                config["services"][name]["volumes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|m| m["target"] == "/keyring")
                    .and_then(|m| m["source"].as_str())
                    .ok_or_else(|| anyhow::anyhow!("{case} {project} {name}: no keyring source"))
            };
            assert_eq!(source("admin")?, source("app")?, "{case} {project}");
            sources.push(source("admin")?.to_string());
        }
        assert_ne!(sources[0], sources[1], "{case}: projects share a keyring bind source");
    }
    Ok(())
}

#[test]
fn compose_app_binds_all_interfaces_as_the_non_owner_role() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    // the image's own ENV (Dockerfile); compose's env_file and environment override it
    let image_env = [
        ("OWNHEALTH_DATA_DIR", "/data"),
        ("OWNHEALTH_HOST", "0.0.0.0"),
        ("OWNHEALTH_PORT", "8080"),
    ];
    for (case, compose, env_file, extra) in cases() {
        let services = render(&compose, &env_file, extra, &[])?;
        let (app, admin) = (&services["app"], &services["admin"]);
        // OWNHEALTH_HOST is the bind address: a public name there crash-loops the app
        let mut env: HashMap<String, String> = image_env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        for (k, v) in app["environment"].as_object().into_iter().flatten() {
            if k.starts_with("OWNHEALTH_") && !k.ends_with("_FILE") {
                env.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
            }
        }
        assert_eq!(Settings::from(&env)?.host, "0.0.0.0", "{case}");
        assert!(
            env_str(app, "OWNHEALTH_DATABASE_URL").starts_with("postgresql://ownhealth_app@"),
            "{case}"
        );
        assert_eq!(env_str(app, "OWNHEALTH_RUN_MIGRATIONS"), "0", "{case}");
        assert_eq!(
            app["depends_on"]["admin"]["condition"], "service_completed_successfully",
            "{case}"
        );
        assert!(
            env_str(admin, "OWNHEALTH_DATABASE_URL").starts_with("postgresql://ownhealth@"),
            "{case}"
        );
        let command: Vec<&str> = admin["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        assert!(command.join(" ").contains("app-role.sh"), "{case}: {command:?}");
    }
    Ok(())
}

/// restic.env holds the backup repository and its S3 keys, for the admin service only. The app is the
/// network-facing process, so a compromise of it must not hand over the keys to the backups. An
/// install made before restic.env existed keeps RESTIC_REPOSITORY in .env and must still render.
#[test]
fn homelab_app_gets_no_backup_credentials() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    let [(_, compose, env_file, _), _] = cases();
    let restic_env = std::fs::read_to_string(root().join("deploy/restic.env.example"))?
        + "AWS_ACCESS_KEY_ID=synthetic-key\nAWS_SECRET_ACCESS_KEY=synthetic-secret\n";
    let services = render(&compose, &env_file, "", &[("restic.env", restic_env)])?;
    let (app, admin) = (&services["app"], &services["admin"]);
    let leaked: Vec<&String> = app["environment"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("RESTIC_") || k.starts_with("AWS_"))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
    assert_eq!(env_str(admin, "AWS_ACCESS_KEY_ID"), "synthetic-key");
    assert!(env_str(admin, "RESTIC_REPOSITORY").starts_with("s3:"));
    assert_eq!(env_str(app, "OWNHEALTH_DEFAULT_LOCALE"), "es");

    let old = render(
        &compose,
        &env_file,
        "RESTIC_REPOSITORY=s3:https://s3.example.lan/old\n",
        &[],
    )?;
    assert_eq!(
        env_str(&old["admin"], "RESTIC_REPOSITORY"),
        "s3:https://s3.example.lan/old"
    );
    Ok(())
}

/// The keyring (slice 47) sits on a bind mount of its own. `admin` (backups and restores) and `app`
/// (profile delete) mount the same host folder at OWNHEALTH_KEYRING_DIR, away from the data volume
/// that holds the blobs and the restic staging folder, so no backup copies it. No other service gets
/// it.
#[test]
fn the_keyring_sits_on_its_own_mount_outside_the_data_volume() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    for (case, compose, env_file, extra) in cases() {
        let services = render(&compose, &env_file, extra, &[])?;
        let mut sources = Vec::new();
        for name in ["admin", "app"] {
            let svc = &services[name];
            let dir = env_str(svc, "OWNHEALTH_KEYRING_DIR");
            assert!(
                dir.starts_with('/') && !dir.starts_with("/data"),
                "{case} {name}: {dir:?}"
            );
            let mounts: Vec<&Value> = svc["volumes"].as_array().into_iter().flatten().collect();
            let own = mounts
                .iter()
                .find(|m| m["target"] == dir.as_str())
                .ok_or_else(|| anyhow::anyhow!("{case} {name}: nothing is mounted at {dir}"))?;
            assert_eq!(own["type"], "bind", "{case} {name}: {own}");
            assert_ne!(own["read_only"], true, "{case} {name}: a delete must rewrite it");
            // no other mount holds it: not the data volume, nor any folder above it
            for m in &mounts {
                let target = m["target"].as_str().unwrap_or_default();
                assert!(
                    std::ptr::eq(*m, *own) || !Path::new(&dir).starts_with(target),
                    "{case} {name}: the keyring is inside {target}"
                );
            }
            sources.push(own["source"].as_str().unwrap_or_default().to_string());
        }
        assert!(
            sources[0] == sources[1] && sources[0].ends_with("/keyring/ownhealth"),
            "{case}: {sources:?}"
        );
        for (name, svc) in services.as_object().into_iter().flatten() {
            if name == "admin" || name == "app" {
                continue;
            }
            let leaked = svc["volumes"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| m["source"] == sources[0].as_str());
            assert!(!leaked, "{case}: {name} mounts the keyring");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- wearables ----

/// The Open Wearables backend image, pinned to a digest after the tag in both compose files.
const OW_IMAGE: &str = "themomentum/open-wearables-backend:0.11.0@sha256:";
/// The long-running sidecar services, all from OW_IMAGE.
const OW_SERVICES: [&str; 3] = ["ow-app", "ow-worker", "ow-beat"];
/// Every service of the profile.
const OW_ALL: [&str; 5] = ["ow-db-init", "ow-redis", "ow-app", "ow-worker", "ow-beat"];
/// What keeps the sidecar quiet: no usage pings, no Sentry, no OTLP export, no outgoing webhooks.
const OW_QUIET: [(&str, &str); 5] = [
    ("TELEMETRY_ENABLED", "false"),
    ("DO_NOT_TRACK", "1"),
    ("SENTRY_ENABLED", "false"),
    ("OTEL_ENABLED", "false"),
    ("OUTGOING_WEBHOOKS_ENABLED", "false"),
];

fn ow_image_is_pinned(image: &str) -> bool {
    image
        .strip_prefix(OW_IMAGE)
        .is_some_and(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// `redis:8.<minor>.<patch>`: an exact release, never `redis:8` or `:latest`.
fn redis_is_pinned(image: &str) -> bool {
    image.strip_prefix("redis:8.").is_some_and(|v| {
        let parts: Vec<&str> = v.split('.').collect();
        parts.len() == 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Null, false, or an empty list or map: a key that grants nothing.
fn unset(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Value::Object(o) => o.keys().cloned().collect(),
        _ => vec![],
    }
}

/// The .env lines that turn the profile on, next to the `ocr` one, for a render or a real start.
fn wearables_env(extra: &str) -> String {
    format!("{extra}COMPOSE_PROFILES=ocr,wearables\n")
}

/// openwearables.env, next to .env in both compose files, with fresh random values.
fn openwearables_env(ow_db_password: &str) -> String {
    let mut env = String::new();
    for (k, v) in [
        ("OPENWEARABLES_DB_PASSWORD", ow_db_password.to_string()),
        ("OPENWEARABLES_SECRET_KEY", ownhealth_core::ids::token_hex(32)),
        ("OPENWEARABLES_ADMIN_PASSWORD", ownhealth_core::ids::token_hex(16)),
    ] {
        env += &format!("{k}={v}\n");
    }
    env
}

/// The profile as both compose files render it: off by default; when on, pinned, quiet, with no
/// published port and none of OwnHealth's secrets, Redis reachable only on an internal network, and
/// its own database and role set up before the sidecar starts.
#[test]
fn compose_wearables_profile_is_pinned_quiet_and_fenced() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    let mut digests = vec![];
    for (case, compose, env_file, extra) in cases() {
        let off = render(&compose, &env_file, extra, &[])?;
        for s in OW_ALL {
            assert!(off.get(s).is_none(), "{case}: {s} starts without the wearables profile");
        }

        let ow_pw = ownhealth_core::ids::token_hex(16);
        let files = [("openwearables.env", openwearables_env(&ow_pw))];
        let doc = render_doc(&compose, &env_file, &wearables_env(extra), &files)?;
        let services = &doc["services"];
        // OW's secrets reach the sidecar only, never OwnHealth's own containers (the OCR worker included)
        for s in ["admin", "app", "ocr"] {
            assert!(services[s].is_object(), "{case}: no {s} service");
            let leaked: Vec<String> = strings(&services[s]["environment"])
                .into_iter()
                .filter(|k| k.starts_with("OPENWEARABLES_"))
                .collect();
            assert!(leaked.is_empty(), "{case}: {s} gets {leaked:?}");
        }
        // ...and the sidecar gets them from openwearables.env, an env file compose may find missing:
        // no OW service holds a docker secret of its own, whose file compose would need at create time
        for s in ["ow-db-init", "ow-app", "ow-worker", "ow-beat"] {
            let got = env_str(&services[s], "OPENWEARABLES_DB_PASSWORD");
            assert_eq!(got, ow_pw, "{case}: {s} OPENWEARABLES_DB_PASSWORD");
        }
        for s in OW_ALL {
            let svc = &services[s];
            assert!(svc.is_object(), "{case}: no {s} service");
            assert_eq!(strings(&svc["profiles"]), ["wearables"], "{case}: {s}");
            // rendered as objects ({target, published, host_ip}), so count entries of any shape
            let ports = &svc["ports"];
            assert!(
                ports.as_array().is_none_or(Vec::is_empty) && !ports.is_object(),
                "{case}: {s} publishes {ports}"
            );
        }
        for s in ["ow-redis", "ow-app", "ow-worker", "ow-beat"] {
            let svc = &services[s];
            let leaked: Vec<String> = strings(&svc["environment"])
                .into_iter()
                .filter(|k| k.starts_with("OWNHEALTH_") || k.starts_with("RESTIC_") || k.starts_with("AWS_"))
                .collect();
            assert!(leaked.is_empty(), "{case}: {s} gets {leaked:?}");
            let secrets: Vec<String> = svc["secrets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v["source"].as_str().or(v.as_str()).map(str::to_string))
                .collect();
            assert!(secrets.is_empty(), "{case}: {s} gets the secrets {secrets:?}");
        }
        for s in OW_SERVICES {
            let svc = &services[s];
            let image = svc["image"].as_str().unwrap_or_default();
            assert!(
                ow_image_is_pinned(image),
                "{case}: {s} image {image:?} is not pinned by digest"
            );
            digests.push(image.to_string());
            for (k, v) in OW_QUIET {
                assert_eq!(env_str(svc, k), v, "{case}: {s} {k}");
            }
            for k in ["DB_NAME", "DB_USER"] {
                let v = env_str(svc, k);
                assert!(!v.is_empty() && v != "ownhealth", "{case}: {s} {k}={v:?}");
            }
            assert_eq!(env_str(svc, "DB_HOST"), "db", "{case}: {s}");
            assert_eq!(env_str(svc, "REDIS_HOST"), "ow-redis", "{case}: {s}");
            assert!(
                strings(&svc["networks"]).contains(&"wearables".to_string()),
                "{case}: {s}"
            );
        }
        // Nothing of the host or of another container reaches the sidecar: no mount but a tmpfs (and
        // ow-db-init's read-only ow-role.sql), no privileges, no capability added, no device, and
        // neither the host's process nor its IPC namespace.
        for s in OW_ALL {
            let svc = &services[s];
            let mounts: Vec<&Value> = svc["volumes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["type"] != "tmpfs")
                .collect();
            let role_sql = |v: &Value| {
                v["type"] == "bind"
                    && v["target"] == "/opt/ow-role.sql"
                    && v["read_only"] == true
                    && v["source"]
                        .as_str()
                        .is_some_and(|p| p.ends_with("/deploy/db/ow-role.sql"))
            };
            let allowed = if s == "ow-db-init" { 1 } else { 0 };
            assert!(
                mounts.len() == allowed && mounts.iter().all(|v| role_sql(v)),
                "{case}: {s} mounts {mounts:?}"
            );
            for k in ["volumes_from", "privileged", "cap_add", "devices"] {
                assert!(unset(&svc[k]), "{case}: {s} {k}: {}", svc[k]);
            }
            for k in ["pid", "ipc", "userns_mode"] {
                assert_ne!(svc[k], "host", "{case}: {s} {k}");
            }
        }
        // The homelab file hardens all five: every capability dropped, no privilege gained, and a
        // read-only root filesystem except for beat, which writes its schedule file.
        if case == "homelab" {
            for s in OW_ALL {
                let svc = &services[s];
                assert_eq!(strings(&svc["cap_drop"]), ["ALL"], "{case}: {s} cap_drop");
                assert!(
                    strings(&svc["security_opt"]).contains(&"no-new-privileges:true".to_string()),
                    "{case}: {s} security_opt {}",
                    svc["security_opt"]
                );
                assert_eq!(svc["read_only"] == true, s != "ow-beat", "{case}: {s} read_only");
            }
        }

        let redis = &services["ow-redis"];
        let image = redis["image"].as_str().unwrap_or_default();
        assert!(redis_is_pinned(image), "{case}: ow-redis image {image:?}");
        assert_eq!(strings(&redis["networks"]), ["wearables"], "{case}: ow-redis networks");
        assert_eq!(
            doc["networks"]["wearables"]["internal"], true,
            "{case}: wearables network"
        );

        // The sidecar shares no network with OwnHealth's own containers except ow-db, which holds db
        // and the sidecar only: OW cannot reach the app (so it cannot forward a client address to it),
        // only Postgres, where its role opens nothing but its own database.
        let nets = |s: &str| strings(&services[s]["networks"]);
        let ours: Vec<String> = services
            .as_object()
            .into_iter()
            .flatten()
            .map(|(s, _)| s.as_str())
            .filter(|s| !OW_ALL.contains(s) && *s != "db")
            .flat_map(&nets)
            .collect();
        assert!(
            ours.contains(&"default".to_string()),
            "{case}: the app's own network: {ours:?}"
        );
        for s in OW_ALL {
            for n in nets(s) {
                assert!(
                    n == "ow-db" || !ours.contains(&n),
                    "{case}: {s} shares {n} with OwnHealth"
                );
            }
        }
        assert!(
            !ours.contains(&"ow-db".to_string()),
            "{case}: an OwnHealth service is on ow-db"
        );
        assert!(nets("db").contains(&"ow-db".to_string()), "{case}: db is not on ow-db");
        assert_eq!(doc["networks"]["ow-db"]["internal"], true, "{case}: ow-db network");
        for s in ["ow-db-init", "ow-app", "ow-worker", "ow-beat"] {
            assert!(nets(s).contains(&"ow-db".to_string()), "{case}: {s} cannot reach db");
        }
        // ow-app and ow-worker call the vendors, so they keep one network with a way out
        for s in ["ow-app", "ow-worker"] {
            assert!(
                nets(s).iter().any(|n| doc["networks"][n.as_str()]["internal"] != true),
                "{case}: {s} has no way out: {:?}",
                nets(s)
            );
        }

        let init = &services["ow-db-init"];
        assert!(strings(&init["command"]).join(" ").contains("ow-role.sql"), "{case}");
        let init_secrets: Vec<String> = init["secrets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["source"].as_str().map(str::to_string))
            .collect();
        let owner_only: &[&str] = if case == "homelab" { &["db_password"] } else { &[] };
        assert_eq!(init_secrets, owner_only, "{case}: ow-db-init secrets");
        assert_eq!(
            init["depends_on"]["app"]["condition"], "service_healthy",
            "{case}: ow-db-init must wait for the app, so that its failure cannot stop it"
        );
        assert_eq!(
            services["ow-app"]["depends_on"]["ow-db-init"]["condition"], "service_completed_successfully",
            "{case}"
        );
        for s in ["ow-worker", "ow-beat"] {
            assert_eq!(
                services[s]["depends_on"]["ow-app"]["condition"], "service_healthy",
                "{case}: {s} must wait for OW's migrations"
            );
        }
    }
    digests.dedup();
    assert_eq!(digests.len(), 1, "both compose files pin the same image: {digests:?}");
    Ok(())
}

/// openwearables.env holds OW's secrets next to .env, so git and the image build skip it as they skip
/// .env (the control).
#[test]
fn openwearables_env_is_ignored_like_dot_env() -> R {
    for f in [".gitignore", ".dockerignore"] {
        let body = std::fs::read_to_string(root().join(f))?;
        for name in [".env", "openwearables.env"] {
            assert!(body.lines().any(|l| l.trim() == name), "{f} does not list {name}");
        }
    }
    Ok(())
}

/// The quickstart makes keyring/ in the clone. git never versions it (an older copy of keyring.json
/// opens a deleted profile's files again) and the image build never reads it. The controls: secrets/
/// and .env, which the two files already list.
#[test]
fn the_keyring_folder_is_ignored_like_secrets() -> R {
    for (f, names) in [
        (".gitignore", ["/secrets/", "/keyring/"]),
        (".dockerignore", [".env", "keyring"]),
    ] {
        let body = std::fs::read_to_string(root().join(f))?;
        for name in names {
            assert!(body.lines().any(|l| l.trim() == name), "{f} does not list {name}");
        }
    }
    Ok(())
}

/// A compose project of its own, taken down with its volumes when dropped.
struct Project {
    name: String,
    dir: tempfile::TempDir,
}

impl Project {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new("docker");
        c.args(["compose", "-p", &self.name])
            .args(args)
            .current_dir(self.dir.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()));
        c
    }

    fn run(&self, args: &[&str]) -> anyhow::Result<Output> {
        Ok(self.cmd(args).stdin(Stdio::null()).output()?)
    }

    fn ok(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = self.run(args)?;
        anyhow::ensure!(
            out.status.success(),
            "docker compose {}: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `docker compose ps -a` as one JSON object per container (older compose prints an array).
    fn ps(&self) -> anyhow::Result<Vec<Value>> {
        let out = self.ok(&["ps", "-a", "--format", "json"])?;
        if out.starts_with('[') {
            return Ok(serde_json::from_str(&out)?);
        }
        Ok(out.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
    }

    /// The `ps -a` entry of one service's container (Null when it has none).
    fn service(&self, s: &str) -> anyhow::Result<Value> {
        Ok(self
            .ps()?
            .into_iter()
            .find(|c| c["Service"] == s)
            .unwrap_or(Value::Null))
    }

    fn logs(&self) -> String {
        self.run(&["logs", "--no-color", "--tail", "60"])
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
            .unwrap_or_default()
    }

    /// psql in the db container to `db`, its compose-network address, as from any other container:
    /// pg_hba trusts loopback only, so this login checks the password.
    fn psql(&self, user: &str, password: &str, db: &str, sql: &str) -> anyhow::Result<Output> {
        self.run(&[
            "exec",
            "-T",
            "-e",
            &format!("PGPASSWORD={password}"),
            "db",
            "psql",
            "-h",
            "db",
            "-U",
            user,
            "-d",
            db,
            "-X",
            "-A",
            "-t",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ])
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.run(&["down", "-v", "--remove-orphans", "--timeout", "5"]);
    }
}

/// Loads PID 1's environment, where the homelab wrapper exported the secrets that `exec` lacks.
macro_rules! pid1_env {
    () => {
        "import os\nos.environ.update(l.split('=', 1) for l in open('/proc/1/environ').read().split('\\0') if '=' in l)\n"
    };
}

/// `alembic current` as the running sidecar's role.
const OW_ALEMBIC_PY: &str = concat!(
    pid1_env!(),
    "from alembic.config import main\nmain(argv=['-c', 'alembic.ini', 'current'])"
);

/// OW's own settings, as the running process sees them; whether beat schedules a usage ping, and
/// (the control) one of its own jobs.
const OW_SETTINGS_PY: &str = concat!(
    pid1_env!(),
    "from app.config import settings as s
from app.main import celery_app
beat = celery_app.conf.beat_schedule or {}
print(s.telemetry_enabled, s.SENTRY_ENABLED, s.otel_enabled, s.outgoing_webhooks_enabled,
      'send-telemetry-ping' in beat, 'roll-up-queued-days' in beat)"
);

/// Tries a TCP connection to each `host:port` argument, 3 s each; prints `host:port open|closed`.
const PROBE_PY: &str = "import socket, sys
for t in sys.argv[1:]:
    h, p = t.rsplit(':', 1)
    try:
        socket.create_connection((h, int(p)), 3).close()
        print(t, 'open')
    except OSError:
        print(t, 'closed')";

/// What an operator's `docker compose up -d` starts, less Caddy (host ports 80 and 443) and the OCR
/// worker (another image).
const UP: [&str; 6] = ["up", "-d", "app", "ow-app", "ow-worker", "ow-beat"];

fn bring_up(case: &str, compose: &Path, env_file: &Path, extra: &str) -> R {
    use ownhealth_core::ids::token_hex;
    let tag = token_hex(4);
    let project = Project {
        name: format!("oh-wear-{case}-{tag}"),
        dir: tempfile::tempdir()?,
    };
    let dir = project.dir.path();
    std::fs::copy(compose, dir.join("compose.yaml"))?;
    std::fs::create_dir_all(dir.join("deploy/db"))?;
    std::fs::create_dir_all(dir.join("inbox"))?;
    std::fs::create_dir_all(dir.join("keyring"))?;
    std::fs::copy(root().join("deploy/db/ow-role.sql"), dir.join("deploy/db/ow-role.sql"))?;
    let ow_pw = token_hex(16);
    let mut env = std::fs::read_to_string(env_file)? + extra;
    let owner_pw = if case == "quickstart" {
        // the app's own key, and any free host port for it
        env += &format!("OWNHEALTH_SECRET_KEY={}\nOWNHEALTH_HTTP_PORT=0\n", token_hex(32));
        // the profile key's root, the one file secret of compose.yaml
        std::fs::create_dir_all(dir.join("secrets"))?;
        std::fs::write(dir.join("secrets/secrets_key"), format!("{}0=", token_hex(21)))?;
        "x".to_string() // OWNHEALTH_DB_PASSWORD in cases()
    } else {
        let s = dir.join("secrets");
        std::fs::create_dir_all(&s)?;
        let owner = token_hex(16);
        for (name, value) in [
            ("db_password", owner.clone()),
            ("app_db_password", token_hex(16)),
            ("secret_key", token_hex(32)),
            ("secrets_key", format!("{}0=", token_hex(21))), // 32 bytes in standard base64
            ("restic_password", token_hex(16)),
        ] {
            std::fs::write(s.join(name), value)?;
        }
        owner
    };
    // OwnHealth runs before the profile is on
    std::fs::write(dir.join(".env"), &env)?;
    project.ok(&["up", "-d", "--wait", "app"])?;
    let before = project.service("app")?;

    // The profile goes on with OW's secret missing (homelab: no openwearables.env) or empty
    // (quickstart). The .env edit recreates the app; the sidecar stops, the new app is up and healthy.
    std::fs::write(dir.join(".env"), env + &wearables_env(""))?;
    if case == "quickstart" {
        std::fs::write(dir.join("openwearables.env"), openwearables_env(""))?;
    }
    let first = project.run(&UP)?;
    let out = String::from_utf8_lossy(&first.stdout).to_string() + &String::from_utf8_lossy(&first.stderr);
    assert!(
        !first.status.success() && out.contains("ow-db-init"),
        "{case}: up with a broken OW secret: {out}"
    );
    let app = project.service("app")?;
    assert!(
        app["State"] == "running" && app["Health"] == "healthy",
        "{case}: the app after a failed ow-db-init: {} {}\n{out}",
        app["State"],
        app["Health"]
    );
    assert_ne!(
        app["ID"], before["ID"],
        "{case}: the profile edit to .env should recreate the app"
    );
    let init_log = project.run(&["logs", "--no-color", "ow-db-init"])?;
    let init_log = String::from_utf8_lossy(&init_log.stdout).to_string() + &String::from_utf8_lossy(&init_log.stderr);
    assert!(
        init_log.contains("OPENWEARABLES_DB_PASSWORD is") && init_log.contains("openwearables.env"),
        "{case}: ow-db-init does not name the missing secret: {init_log}"
    );
    // the secret set: the next start brings the sidecar up and leaves the app as it was
    std::fs::write(dir.join("openwearables.env"), openwearables_env(&ow_pw))?;
    project.ok(&UP)?;
    assert_eq!(
        project.service("app")?["ID"],
        app["ID"],
        "{case}: setting OW's secret recreated the app"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(480);
    loop {
        let ps = project.ps()?;
        let get = |s: &str| ps.iter().find(|c| c["Service"] == s).cloned().unwrap_or(Value::Null);
        let init = get("ow-db-init");
        if init["State"] == "exited" && init["ExitCode"] != 0 {
            anyhow::bail!("{case}: ow-db-init failed\n{}", project.logs());
        }
        for s in OW_SERVICES {
            if get(s)["State"] == "exited" {
                anyhow::bail!("{case}: {s} exited\n{}", project.logs());
            }
        }
        if get("ow-app")["Health"] == "healthy" && OW_SERVICES.iter().all(|s| get(s)["State"] == "running") {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "{case}: ow-app not healthy in time\n{}",
            project.logs()
        );
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    // OW migrated its own database to head, as its own role
    let current = project.ok(&["exec", "-T", "ow-app", "python", "-c", OW_ALEMBIC_PY])?;
    assert!(current.contains("(head)"), "{case}: alembic current: {current}");
    let owner = |sql: &str| -> anyhow::Result<String> {
        let out = project.psql("ownhealth", &owner_pw, "openwearables", sql)?;
        anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    assert_eq!(
        owner(
            "select count(*) > 20, bool_and(tableowner = 'openwearables') from pg_tables where schemaname = 'public'"
        )?,
        "t|t",
        "{case}: OW's tables, owned by its role"
    );
    assert_eq!(
        owner(
            "select rolsuper, rolcreatedb, rolcreaterole, rolreplication, rolbypassrls from pg_roles \
             where rolname = 'openwearables'"
        )?,
        "f|f|f|f|f",
        "{case}: the OW role's attributes"
    );
    assert_eq!(
        owner(
            "select string_agg(datname, ',' order by datname) from pg_database \
             where datallowconn and has_database_privilege('openwearables', oid, 'CONNECT')"
        )?,
        "openwearables",
        "{case}: the OW role connects to its own database only"
    );

    // the OW role reaches its own database with its password and not with another one, and is
    // refused at the health database's door
    let own = project.psql("openwearables", &ow_pw, "openwearables", "select 1")?;
    assert!(own.status.success(), "{case}: {}", String::from_utf8_lossy(&own.stderr));
    let wrong = project.psql("openwearables", &token_hex(16), "openwearables", "select 1")?;
    let err = String::from_utf8_lossy(&wrong.stderr);
    assert!(
        !wrong.status.success() && err.contains("password authentication failed"),
        "{case}: the OW role got in with a wrong password: {err}"
    );
    let health = project.psql("openwearables", &ow_pw, "ownhealth", "select 1")?;
    let err = String::from_utf8_lossy(&health.stderr);
    assert!(
        !health.status.success() && err.contains("permission denied for database"),
        "{case}: the OW role reached the health database: {err}"
    );

    // quiet, as the running processes see their settings; nothing published on the host
    for s in OW_SERVICES {
        let settings = project.ok(&["exec", "-T", s, "python", "-c", OW_SETTINGS_PY])?;
        assert_eq!(
            settings.lines().last().unwrap_or_default(),
            "False False False False False True", // the last one: beat's own schedule is there
            "{case}: {s} settings"
        );
    }
    // off OwnHealth's network: from each sidecar container, db answers, and the app answers neither by
    // its name nor by any of its addresses
    let app_id = project.service("app")?["ID"].as_str().unwrap_or_default().to_string();
    let inspect = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
            &app_id,
        ])
        .output()?;
    anyhow::ensure!(inspect.status.success(), "{}", String::from_utf8_lossy(&inspect.stderr));
    let app_ips: Vec<String> = String::from_utf8_lossy(&inspect.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    anyhow::ensure!(!app_ips.is_empty(), "{case}: the app has no address");
    let mut targets = vec!["db:5432".to_string(), "app:8080".to_string()];
    targets.extend(app_ips.iter().map(|ip| format!("{ip}:8080")));
    let want: Vec<String> = targets
        .iter()
        .map(|t| format!("{t} {}", if t == "db:5432" { "open" } else { "closed" }))
        .collect();
    for s in OW_SERVICES {
        let mut args = vec!["exec", "-T", s, "python", "-c", PROBE_PY];
        args.extend(targets.iter().map(String::as_str));
        let got = project.ok(&args)?;
        assert_eq!(got.lines().collect::<Vec<_>>(), want, "{case}: what {s} reaches");
    }

    for c in project
        .ps()?
        .into_iter()
        .filter(|c| OW_ALL.iter().any(|s| c["Service"] == *s))
    {
        let published: Vec<&Value> = c["Publishers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["PublishedPort"].as_u64().unwrap_or(0) != 0)
            .collect();
        assert!(published.is_empty(), "{case}: {} publishes {published:?}", c["Service"]);
    }
    Ok(())
}

/// The profile for real, from both compose files: turning it on with OW's secret missing or empty
/// stops the sidecar and leaves the (recreated) app up, setting the secret later does not recreate
/// the app, OW's migrations run on Postgres 17 as its restricted role, the role cannot connect to the
/// health database, the running sidecar has telemetry, Sentry, OTLP and webhooks off, it reaches db
/// but not the app, and nothing is published on the host. It pulls the OW image and takes minutes,
/// so it runs in CI (`CI` set) or with OWNHEALTH_TEST_WEARABLES=1.
#[test]
fn wearables_profile_comes_up_as_the_restricted_role() -> R {
    let wanted = std::env::var_os("CI").is_some() || std::env::var_os("OWNHEALTH_TEST_WEARABLES").is_some();
    if !wanted || !has_compose() {
        eprintln!("set OWNHEALTH_TEST_WEARABLES=1 (and have docker compose) to bring the profile up: skipped");
        return Ok(());
    }
    for (case, compose, env_file, extra) in cases() {
        bring_up(case, &compose, &env_file, extra)?;
    }
    Ok(())
}

// -------------------------------------------------------------- share block ----

/// The caddy image compose.homelab.yaml runs.
fn caddy_image() -> anyhow::Result<String> {
    let compose = std::fs::read_to_string(root().join("deploy/compose.homelab.yaml"))?;
    compose
        .lines()
        .find_map(|l| l.trim().strip_prefix("image: caddy:").map(|tag| format!("caddy:{tag}")))
        .ok_or_else(|| anyhow::anyhow!("no caddy image in compose.homelab.yaml"))
}

/// A container that is removed when dropped, whatever happened.
struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

/// deploy/Caddyfile in `image`, with a stub on app:8080 that names each request it receives and the
/// X-Forwarded-For it came with. Returns the container and the host address of the share port.
fn caddy(image: &str) -> anyhow::Result<(Container, String)> {
    let name = format!("oh-share-block-{}", uuid::Uuid::new_v4().simple());
    let caddyfile = std::fs::read_to_string(root().join("deploy/Caddyfile"))?
        + "\nhttp://:8080 {\n\trespond \"upstream {method} {uri} xff={header.X-Forwarded-For}\" 200\n}\n";
    let mut create = Command::new("docker");
    create.args(["create", "--name", &name, "--add-host", "app:127.0.0.1"]);
    create.args([
        "-p",
        "127.0.0.1::8090",
        "-e",
        "OWNHEALTH_DOMAIN=http://private.test:8081",
    ]);
    let out = create.arg(image).output()?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let c = Container(name.clone());
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("Caddyfile");
    std::fs::write(&file, caddyfile)?;
    let target = format!("{name}:/etc/caddy/Caddyfile");
    anyhow::ensure!(
        ok(Command::new("docker").args(["cp"]).arg(&file).arg(&target)),
        "docker cp"
    );
    anyhow::ensure!(ok(Command::new("docker").args(["start", &name])), "docker start");
    let port = Command::new("docker").args(["port", &name, "8090/tcp"]).output()?;
    let addr = String::from_utf8(port.stdout)?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    anyhow::ensure!(!addr.is_empty(), "no published port");
    for _ in 0..100 {
        if fetch(&addr, "GET", "/")?.0 != 0 {
            return Ok((c, addr));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let logs = Command::new("docker").args(["logs", &name]).output()?;
    anyhow::bail!("caddy did not answer: {}", String::from_utf8_lossy(&logs.stderr))
}

/// (status, body) of one request through curl; status 0 when nothing answered.
fn fetch(addr: &str, method: &str, path: &str) -> anyhow::Result<(u16, String)> {
    fetch_with(addr, method, path, &[])
}

fn fetch_with(addr: &str, method: &str, path: &str, headers: &[&str]) -> anyhow::Result<(u16, String)> {
    // --path-as-is: curl would otherwise resolve a `..` itself before sending; --noproxy: an
    // http_proxy in the environment must not answer for the container on loopback
    let mut curl = Command::new("curl");
    curl.args([
        "-s",
        "--path-as-is",
        "--noproxy",
        "*",
        "-m",
        "5",
        "-o",
        "-",
        "-w",
        "\n%{http_code}",
    ]);
    for h in headers {
        curl.args(["-H", h]);
    }
    if method == "HEAD" {
        curl.arg("-I");
    } else {
        curl.args(["-X", method]);
    }
    let out = curl.arg(format!("http://{addr}{path}")).output()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    Ok((code.trim().parse().unwrap_or(0), body.to_string()))
}

/// The share host exposes the three link routes and nothing else (security-privacy.md section 11).
/// Caddy matches the cleaned path, so every raw path that cleans to a link route (`..`, `//`, `./`,
/// `%2F`, an encoded letter) must reach the app as the canonical route and nothing else, never as
/// the raw path the app would route elsewhere (the UI's login page, the calendar feed).
#[test]
fn the_public_share_block_passes_only_the_link_routes() -> R {
    if !ok(Command::new("docker").args(["info"])) {
        eprintln!("docker not available: skipped");
        return Ok(());
    }
    let image = caddy_image()?;
    let have = ok(Command::new("docker").args(["image", "inspect", &image]));
    anyhow::ensure!(
        have || ok(Command::new("docker").args(["pull", &image])),
        "docker pull {image}"
    );
    let id = "I91rhba3VsuGXGchcnr6VHlQFKxfE28kuZ0ssbEuxno";
    let t = "Y9xwkUdtmN9wwoJoN3ffJIhX2UGvCL1JnlPVNL3kDWM";
    let m = format!("/api/shl/{id}");
    let f = format!("/api/shl/{id}/file/{t}");
    let ice = format!("{m}/ice");
    let direct = format!("{ice}?recipient=LabNorte");
    // (method, raw path, the request line the app must get)
    let mut through: Vec<(&str, String, String)> = vec![
        ("POST", m.clone(), format!("POST {m}")),
        ("OPTIONS", m.clone(), format!("OPTIONS {m}")),
        ("GET", f.clone(), format!("GET {f}")),
        ("GET", direct.clone(), format!("GET {direct}")),
        ("GET", format!("/x/..{direct}"), format!("GET {direct}")),
        ("GET", format!("{f}?x=1"), format!("GET {f}")),
        ("POST", format!("{m}?x=1"), format!("POST {m}")),
        ("POST", format!("//api/shl/{id}"), format!("POST {m}")),
        ("POST", format!("/api//shl/{id}"), format!("POST {m}")),
        ("POST", format!("/./api/shl/{id}"), format!("POST {m}")),
        ("POST", format!("/api/shl/./{id}"), format!("POST {m}")),
        ("POST", format!("/x/../api/shl/{id}"), format!("POST {m}")),
        ("POST", format!("/api%2Fshl%2F{id}"), format!("POST {m}")),
        ("POST", format!("/api/%73hl/{id}"), format!("POST {m}")),
        ("GET", format!("/login/../api/shl/{id}/file/{t}"), format!("GET {f}")),
        ("GET", format!("/./api/shl/{id}/file/{t}"), format!("GET {f}")),
        ("GET", format!("/api/shl/{id}%2Ffile%2F{t}"), format!("GET {f}")),
        (
            "GET",
            format!("/api/ics/x%2F..%2F..%2Fshl%2F{id}%2Ffile%2F{t}"),
            format!("GET {f}"),
        ),
        (
            "POST",
            format!("/api/shl/%41{}", &id[1..]),
            format!("POST /api/shl/A{}", &id[1..]),
        ),
    ];
    through.sort();
    let mut refused: Vec<(&str, String)> = Vec::new();
    for p in [
        "/".to_string(),
        "/login".into(),
        "/index.html".into(),
        "/_app/version.json".into(),
        "/mcp".into(),
        "/healthz".into(),
        "/metrics".into(),
        "/api".into(),
        "/api/".into(),
        "/api/me".into(),
        "/api/auth/state".into(),
        "/api/auth/login/begin".into(),
        "/api/profiles".into(),
        "/api/profiles/me/shl".into(),
        "/api/ics/oh_ics_abcdefgh_x.ics".into(),
        "/dicom-web".into(),
        "/dicom-web/studies".into(),
        "/ow/api/v1/oauth/oura/callback?code=synthetic".into(),
        "/api/shl".into(),
        "/api/shl/".into(),
        "/api/shl/short".into(),
        format!("{m}/"),
        format!("{m}/extra"),
        format!("{m}/file"),
        format!("{m}/file/"),
        format!("{m}/file/short"),
        format!("{f}/"),
        format!("{f}/x"),
        format!("/API/shl/{id}"),
        format!("{m};x"),
        format!("{m}/../../me"),
        format!("{f}/../../../../me"),
        format!("/api/shl/{}%2F..%2Fme", &id[..40]),
        format!("/api/shl/{}%2F", &id[..41]),
        "/api/shl/%2e%2e%2fprofiles%2fme%2fshl".into(),
        "/api/shl/..%2F..%2Fapi%2Fme".into(),
    ] {
        refused.push(("GET", p.clone()));
        refused.push(("POST", p));
    }
    for method in ["GET", "PUT", "PATCH", "DELETE", "HEAD"] {
        refused.push((method, m.clone()));
    }
    for method in ["POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"] {
        refused.push((method, f.clone()));
        refused.push((method, ice.clone()));
    }
    let (_c, addr) = caddy(&image)?;
    for (method, path, line) in &through {
        let (code, body) = fetch(&addr, method, path)?;
        let got = body.split(" xff=").next().unwrap_or_default();
        assert_eq!(
            (code, got),
            (200, format!("upstream {line}").as_str()),
            "{method} {path}"
        );
    }
    for (method, path) in &refused {
        let (code, body) = fetch(&addr, method, path)?;
        assert_eq!(code, 404, "{method} {path}: {body}");
        assert!(!body.contains("upstream"), "{method} {path}: {body}");
    }
    // the tunnel's X-Forwarded-For goes on, with the hop Caddy saw after it; the app trusts that
    // chain only from the proxies in OWNHEALTH_TRUSTED_PROXIES
    let (_, body) = fetch_with(&addr, "POST", &m, &["X-Forwarded-For: 203.0.113.9"])?;
    let xff = body.split(" xff=").nth(1).unwrap_or_default();
    assert!(xff.starts_with("203.0.113.9, "), "{body}");
    Ok(())
}
