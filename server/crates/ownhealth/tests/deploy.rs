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
    assert!(n >= 1);
    sqlx::query("select count(*) from profile").execute(&mut app).await?; // ordinary tables stay usable
    for sql in [
        "update audit_event set action = 'rewritten'",
        "delete from audit_event",
        "truncate audit_event",
    ] {
        let e = refused(&mut app, sql).await?;
        assert!(e.contains("permission denied"), "{sql}: {e}");
    }
    for sql in [
        "alter table audit_event disable trigger all",
        "drop trigger audit_event_no_update on audit_event",
    ] {
        let e = refused(&mut app, sql).await?;
        assert!(e.contains("must be owner"), "{sql}: {e}");
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
    let out = Command::new("docker")
        .args(["compose", "config", "--format", "json"])
        .current_dir(dir.path())
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
/// The only docker secrets a sidecar service may hold (homelab).
const OW_SECRETS: [&str; 3] = ["ow_db_password", "ow_secret_key", "ow_admin_password"];

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

/// The quickstart's openwearables.env, with fresh random values. The homelab reads secrets/ instead.
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
        let files = if case == "quickstart" {
            vec![("openwearables.env", openwearables_env(&ow_pw))]
        } else {
            vec![]
        };
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
        // ...and the quickstart sidecar gets them from openwearables.env (the homelab reads secrets/)
        for s in ["ow-db-init", "ow-app", "ow-worker", "ow-beat"] {
            let got = env_str(&services[s], "OPENWEARABLES_DB_PASSWORD") == ow_pw;
            assert_eq!(got, case == "quickstart", "{case}: {s} OPENWEARABLES_DB_PASSWORD");
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
            assert!(
                secrets.iter().all(|x| OW_SECRETS.contains(&x.as_str())),
                "{case}: {s} gets the secrets {secrets:?}"
            );
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
        let redis = &services["ow-redis"];
        let image = redis["image"].as_str().unwrap_or_default();
        assert!(redis_is_pinned(image), "{case}: ow-redis image {image:?}");
        assert_eq!(strings(&redis["networks"]), ["wearables"], "{case}: ow-redis networks");
        assert_eq!(
            doc["networks"]["wearables"]["internal"], true,
            "{case}: wearables network"
        );

        let init = &services["ow-db-init"];
        assert!(strings(&init["command"]).join(" ").contains("ow-role.sql"), "{case}");
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
        Ok(self.ps()?.into_iter().find(|c| c["Service"] == s).unwrap_or(Value::Null))
    }

    fn logs(&self) -> String {
        self.run(&["logs", "--no-color", "--tail", "60"])
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
            .unwrap_or_default()
    }

    /// psql in the db container, over TCP so the password is checked.
    fn psql(&self, user: &str, password: &str, db: &str, sql: &str) -> anyhow::Result<Output> {
        self.run(&[
            "exec",
            "-T",
            "-e",
            &format!("PGPASSWORD={password}"),
            "db",
            "psql",
            "-h",
            "127.0.0.1",
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
    std::fs::copy(root().join("deploy/db/ow-role.sql"), dir.join("deploy/db/ow-role.sql"))?;
    let ow_pw = token_hex(16);
    let mut env = std::fs::read_to_string(env_file)? + &wearables_env(extra);
    // OW's secret is missing at first (no openwearables.env, an empty secrets/ow_db_password)
    let owner_pw = if case == "quickstart" {
        // the app's own key, and any free host port for it
        env += &format!("OWNHEALTH_SECRET_KEY={}\nOWNHEALTH_HTTP_PORT=0\n", token_hex(32));
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
            ("ow_db_password", String::new()),
            ("ow_secret_key", token_hex(32)),
            ("ow_admin_password", token_hex(16)),
        ] {
            std::fs::write(s.join(name), value)?;
        }
        owner
    };
    std::fs::write(dir.join(".env"), env)?;

    // A broken OW secret stops the sidecar only: the app is up and healthy, and stays so
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
    // the secret set: the next start brings the sidecar up and leaves the app as it was
    if case == "quickstart" {
        std::fs::write(dir.join("openwearables.env"), openwearables_env(&ow_pw))?;
    } else {
        std::fs::write(dir.join("secrets/ow_db_password"), &ow_pw)?;
    }
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

    // the OW role reaches its own database and is refused at the health database's door
    let own = project.psql("openwearables", &ow_pw, "openwearables", "select 1")?;
    assert!(own.status.success(), "{case}: {}", String::from_utf8_lossy(&own.stderr));
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
    for c in project.ps()?.into_iter().filter(|c| OW_ALL.iter().any(|s| c["Service"] == *s)) {
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

/// The profile for real, from both compose files: a missing OW secret stops the sidecar and leaves the
/// app up, setting it later does not recreate the app, OW's migrations run on Postgres 17 as its
/// restricted role, the role cannot connect to the health database, the running sidecar has
/// telemetry, Sentry, OTLP and webhooks off, and nothing is published on the host. It pulls the OW
/// image and takes minutes, so it runs in CI (`CI` set) or with OWNHEALTH_TEST_WEARABLES=1.
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
