//! v1.0 slice 24: a GEDCOM file is read in memory and never stored. After a preview, an import, an
//! accept and a dismiss of a tree full of distinctive NAME, NOTE, PLAC, source and file-name strings,
//! none of them is in any table, in the blob directory or in the logs. Its own test binary: tracing
//! caches callsite interest globally, so a test that installs a thread-local subscriber races with any
//! other test in the same binary.

use std::path::Path;

use axum::body::Body;
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const F551: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../fixtures/gedcom/rivera-551.ged"
));
const F70: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../fixtures/gedcom/rivera-70.ged"
));
const BOUNDARY: &str = "ownhealthgedcomboundary";
/// In the fixtures: every NAME (Vexlorn), NOTE, CONT, source text and cause of death (Zirkhaven), PLAC
/// (Quillmoor). The file name sent with the upload: Brannoch.
const CANARIES: [&str; 4] = ["Vexlorn", "Zirkhaven", "Quillmoor", "Brannoch"];

/// A writer the test reads back.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut v) = self.0.lock() {
            v.extend_from_slice(b);
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn form(file: &[u8], me: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(me) = me {
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"self\"\r\n\r\n{me}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"Brannoch tree.ged\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

struct Caller {
    rt: Runtime,
    secret: String,
}

impl Caller {
    async fn send(
        &self,
        method: &str,
        uri: &str,
        body: Body,
        content_type: &str,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, format!("{COOKIE}={}", self.secret))
            .header(header::CONTENT_TYPE, content_type)
            .body(body)?;
        let res = ownhealth_api::router().with_state(self.rt.clone()).oneshot(req).await?;
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 26).await?;
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }

    async fn upload(&self, uri: &str, file: &[u8], me: Option<&str>) -> anyhow::Result<(StatusCode, Value)> {
        let ct = format!("multipart/form-data; boundary={BOUNDARY}");
        self.send("POST", uri, Body::from(form(file, me)), &ct).await
    }

    async fn json(&self, method: &str, uri: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
        self.send(method, uri, Body::from(body.to_string()), "application/json")
            .await
    }
}

/// Every row of every table (and every bytea column as hex) that holds one of `needles`.
async fn rows_holding(pool: &PgPool, needles: &[&str]) -> anyhow::Result<Vec<String>> {
    let tables: Vec<(String, String)> = sqlx::query_as(
        "select table_schema, table_name from information_schema.tables where table_type = 'BASE TABLE' \
         and table_schema not in ('pg_catalog', 'information_schema') order by 1, 2",
    )
    .fetch_all(pool)
    .await?;
    let mut found = Vec::new();
    for (schema, table) in tables {
        for needle in needles {
            let hex: String = needle.bytes().map(|b| format!("{b:02x}")).collect();
            let sql = format!(
                "select count(*) from \"{schema}\".\"{table}\" t where t::text ilike '%' || $1 || '%' \
                 or t::text ilike '%' || $2 || '%'"
            );
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(needle)
                .bind(&hex)
                .fetch_one(pool)
                .await?;
            if n > 0 {
                found.push(format!("{schema}.{table}: {needle} x{n}"));
            }
        }
    }
    Ok(found)
}

/// Every file under `dir`, with its bytes.
fn files(dir: &Path) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                todo.push(p);
            } else {
                out.push((p.display().to_string(), std::fs::read(&p)?));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn holds(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn names_notes_and_places_of_a_tree_reach_no_table_no_file_and_no_log() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    let secret = create_session(&mut c, family.user("alex"), None, None, "passkey").await?;
    drop(c);
    let alex = Caller { rt: rt.clone(), secret };
    for f in [F551, F70] {
        let text = String::from_utf8_lossy(f);
        for canary in &CANARIES[..3] {
            assert!(text.contains(canary), "the fixture lost its {canary}");
        }
    }
    let files_before = files(dir.path())?;
    assert!(rows_holding(&db.pool, &CANARIES).await?.is_empty());

    let out = Captured::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    for (file, me) in [(F551, "0"), (F70, "3")] {
        let (s, v) = alex.upload("/profiles/alex/family/gedcom/preview", file, None).await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        let (s, v) = alex.upload("/profiles/alex/family/gedcom", file, Some(me)).await?;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        for canary in CANARIES {
            assert!(!v.to_string().contains(canary), "{v}");
        }
    }
    // a refused file and a refused position say no value either
    let bad = b"0 HEAD\n1 NOTE Zirkhaven\n0 @X@ INDI\n1 NAME Vexlorn\n3 PLAC Quillmoor\n0 TRLR\n";
    let (s, v) = alex.upload("/profiles/alex/family/gedcom/preview", bad, None).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, v) = alex.upload("/profiles/alex/family/gedcom", F551, Some("99")).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (_, list) = alex
        .json("GET", "/review?kind=family_proposal&profile=alex", Value::Null)
        .await?;
    let items = list["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 16, "{list}");
    let id = |n: usize| items.get(n).and_then(|i| i["id"].as_str()).unwrap_or("").to_string();
    let (s, v) = alex
        .json("POST", &format!("/review/{}/accept", id(0)), json!({}))
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = alex
        .json(
            "POST",
            &format!("/review/{}/dismiss", id(1)),
            json!({"reason": "not in this family"}),
        )
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, family_list) = alex.json("GET", "/profiles/alex/family", Value::Null).await?;
    assert_eq!(s, StatusCode::OK);
    for canary in CANARIES {
        assert!(!family_list.to_string().contains(canary), "{family_list}");
    }

    // a control: the capture sees what is logged while the requests ran
    tracing::info!("gedcom-control-line");
    let logs = String::from_utf8(out.0.lock().map(|v| v.clone()).unwrap_or_default())?;
    assert!(logs.contains("gedcom-control-line"), "the log capture saw nothing");
    for canary in CANARIES {
        assert!(!logs.contains(canary), "{canary} in the logs:\n{logs}");
    }

    let leaked = rows_holding(&db.pool, &CANARIES).await?;
    assert!(leaked.is_empty(), "{leaked:?}");
    // no new file at all, so no file can hold the tree, sealed or not
    let files_after = files(dir.path())?;
    let names = |l: &[(String, Vec<u8>)]| l.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    assert_eq!(names(&files_after), names(&files_before));
    for (name, bytes) in &files_after {
        for canary in CANARIES {
            assert!(!holds(bytes, canary), "{canary} in {name}");
        }
    }

    // a control: the table scan finds a value a person typed in by hand
    let (s, v) = alex
        .json(
            "POST",
            "/profiles/alex/family",
            json!({"relationship": "BRO", "display_name": "Vexlorn"}),
        )
        .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert!(
        rows_holding(&db.pool, &["Vexlorn"])
            .await?
            .iter()
            .any(|r| r.starts_with("public.family_member:")),
        "the table scan cannot see a stored name"
    );
    Ok(())
}
