//! Deleting a profile removes every file and stored credential it owns: genome files (gVCF, chip
//! sites and their indexes), OCR word layers, raw device pushes, mail held on its own mailbox, and the
//! sealed passwords and tokens of its inboxes. A file another profile still names stays, and so does
//! a household mailbox with its password.

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeSet;
use std::path::Path;

use axum::http::{Method, StatusCode};
use ownhealth_core::Runtime;
use ownhealth_core::blobs::sha256_hex_to_bytes;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::provenance::{SrcIn, create_src};
use ownhealth_core::secrets::{SecretBox, SecretString, store_secret};
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

/// 32 bytes of 0x0b, base64. Test-only key.
const KEY: &str = "CwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCws=";

/// Every file in the blob store, by its sha256 hex name.
fn stored(root: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, out: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                out.insert(name.to_string());
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&root.join("sha256"), &mut out);
    out
}

/// A new inbox through the REST route; returns its id.
async fn add(me: &mut support::Client, body: Value) -> anyhow::Result<String> {
    let r = me.post("/api/inboxes", &body).await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Ok(r.json["id"].as_str().unwrap_or_default().to_string())
}

/// One genome import on `profile` with the given blob columns set.
async fn genome(
    conn: &mut PgConnection,
    profile: Uuid,
    user: Uuid,
    assay: &str,
    blobs: &[(&str, &str)],
) -> anyhow::Result<()> {
    let src = create_src(
        &mut *conn,
        &SrcIn {
            profile_id: Some(profile),
            ..SrcIn::manual(Some(user))
        },
    )
    .await?;
    let mut cols = vec!["profile_id", "sample_id", "assay", "facts_schema", "facts_sha256"];
    cols.extend(["owngenome_version", "db_versions", "src_id"]);
    cols.extend(blobs.iter().map(|(c, _)| *c));
    let marks: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "insert into genome_dataset ({}) values ({})",
        cols.join(", "),
        marks.join(", ")
    );
    let facts = blobs
        .iter()
        .find(|(c, _)| *c == "facts_blob")
        .map(|(_, s)| sha256_hex_to_bytes(s))
        .transpose()?
        .unwrap_or_default();
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(profile)
        .bind("RIVERA-SYN-01")
        .bind(assay)
        .bind("ownhealth.genome-facts/v1")
        .bind(facts)
        .bind("0.9.0")
        .bind(json!({}))
        .bind(src);
    for (_, sha) in blobs {
        q = q.bind(sha256_hex_to_bytes(sha)?);
    }
    q.execute(&mut *conn).await?;
    Ok(())
}

#[tokio::test]
async fn deleting_a_profile_leaves_none_of_its_files_or_credentials() -> R {
    let mut env = Env::new(true).await?;
    env.settings.secrets_key = Some(KEY.into());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    let sb = SecretBox::from_base64(KEY)?;
    let (alex, sam, user, household) = (
        env.profile("alex"),
        env.profile("sam"),
        env.user("alex"),
        env.fam().household_id,
    );
    let put = |bytes: &str| env.rt.blobs.put_bytes(bytes.as_bytes());
    let mut c = env.pool().acquire().await?;

    // a WGS import with its gVCF and index; Sam's import holds the very same gVCF file
    let gvcf = put("##fileformat=VCFv4.2 synthetic gVCF shared by two imports")?;
    genome(
        &mut c,
        alex,
        user,
        "wgs",
        &[
            ("facts_blob", &put("{\"facts\": \"alex wgs\"}")?),
            ("gvcf_blob", &gvcf),
            ("gvcf_tbi_blob", &put("synthetic tbi of alex's gVCF")?),
        ],
    )
    .await?;
    genome(
        &mut c,
        sam,
        env.user("sam"),
        "wgs",
        &[("facts_blob", &put("{\"facts\": \"sam wgs\"}")?), ("gvcf_blob", &gvcf)],
    )
    .await?;
    // a chip import with the file of every probed site and its index
    genome(
        &mut c,
        alex,
        user,
        "chip",
        &[
            ("facts_blob", &put("{\"facts\": \"alex chip\"}")?),
            ("sites_blob", &put("synthetic chip sites")?),
            ("sites_tbi_blob", &put("synthetic chip sites index")?),
        ],
    )
    .await?;

    // a scanned LabNorte page and the word layer our OCR read from it
    let scan = put("%PDF-1.7 synthetic LabNorte scan")?;
    let doc = create_document(
        &mut c,
        &NewDocument {
            filename: Some("labnorte.pdf"),
            pages: Some(1),
            ..NewDocument::upload(household, Some(alex), &scan, "application/pdf", 32)
        },
    )
    .await?;
    sqlx::query(
        "insert into ocr_run (document_id, page_no, engine, engine_version, models_sha256, status, words_blob) \
         values ($1, 1, 'paddleocr-onnx', 'test', 'test', 'done', $2)",
    )
    .bind(doc.id)
    .bind(sha256_hex_to_bytes(&put("[{\"t\": \"Colesterol\"}]")?)?)
    .execute(&mut *c)
    .await?;
    drop(c);

    let mut me = env.as_user(user).await?;
    let mailbox = |profile: Option<Uuid>, user: &str| {
        json!({"kind": "email", "label": format!("Correo {user}"), "profile_id": profile,
               "secret": "imap-pass-not-real", "config": {"host": "imap.rivera.example", "port": 993,
               "tls": "implicit", "username": user}})
    };
    // Alex's own mailbox, a household mailbox (both with an IMAP password), Alex's Nightscout with
    // its token, and Alex's Health Auto Export inbox
    let own_mail = add(&mut me, mailbox(Some(alex), "alex.rivera")).await?;
    let home_mail = add(&mut me, mailbox(None, "salud.rivera")).await?;
    add(
        &mut me,
        json!({"kind": "nightscout", "label": "Sensor de Alex", "profile_id": alex,
               "secret": "ns-token-not-real", "config": {"base_url": "https://ns.rivera.example"}}),
    )
    .await?;
    let hae = add(
        &mut me,
        json!({"kind": "health_auto_export", "label": "iPhone de Alex", "profile_id": alex}),
    )
    .await?;
    let token = me.post0(&format!("/api/inboxes/{hae}/token")).await?;
    assert_eq!(token.status, StatusCode::OK, "{}", token.json);
    let bearer = format!("Bearer {}", token.json["token"].as_str().unwrap_or_default());
    let r = env
        .anon()
        .send(
            Method::POST,
            "/api/ingest/health-auto-export",
            Some(&json!({"data": {"metrics": []}})),
            &[("authorization", &bearer)],
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    let mut c = env.pool().acquire().await?;
    // mail held on each mailbox; a sender rule on the household mailbox that files to Alex, with the
    // PDF password it tries
    for (source, eml) in [
        (&own_mail, "From: x@unknown.example\r\n\r\nheld for Alex"),
        (&home_mail, "From: y@unknown.example\r\n\r\nheld for the household"),
    ] {
        sqlx::query(
            "insert into quarantine_item (household_id, source_id, raw_blob, reason) \
             values ($1, $2::uuid, $3, 'sender_not_allowed')",
        )
        .bind(household)
        .bind(source)
        .bind(sha256_hex_to_bytes(&put(eml)?)?)
        .execute(&mut *c)
        .await?;
    }
    let pdf_pass = store_secret(
        &mut c,
        &sb,
        household,
        "pdf_password",
        Some("DNI de Alex"),
        &SecretString::new("pdf-pass-not-real"),
    )
    .await?;
    let rule: Uuid = sqlx::query_scalar(
        "insert into sender_rule (source_id, pattern, profile_id) values ($1::uuid, 'labnorte.example', $2) \
         returning id",
    )
    .bind(&home_mail)
    .bind(alex)
    .fetch_one(&mut *c)
    .await?;
    sqlx::query("insert into pdf_password_rule (sender_rule_id, secret_id) values ($1, $2)")
        .bind(rule)
        .bind(pdf_pass)
        .execute(&mut *c)
        .await?;
    drop(c);

    let home_secret: Uuid = sqlx::query_scalar("select secret_id from source where id = $1::uuid")
        .bind(&home_mail)
        .fetch_one(env.pool())
        .await?;
    let home_eml = env
        .rt
        .blobs
        .put_bytes(b"From: y@unknown.example\r\n\r\nheld for the household")?;
    let before = stored(env.rt.blobs.root());
    assert_eq!(before.len(), 12, "{before:?}");
    assert_eq!(env.scalar_i64("select count(*) from secret").await?, 4);

    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    // what stays: the gVCF Sam's import names, Sam's facts, and the household mailbox's mail
    let after = stored(env.rt.blobs.root());
    let sams_facts: String =
        sqlx::query_scalar("select encode(facts_blob, 'hex') from genome_dataset where profile_id = $1")
            .bind(sam)
            .fetch_one(env.pool())
            .await?;
    assert_eq!(after, BTreeSet::from([gvcf, sams_facts, home_eml]));
    assert_eq!(r.json["blobs"], json!(before.len() - 3), "{}", r.json);
    // what stays of the credentials: the household mailbox's password, nothing of Alex's
    let left: Vec<Uuid> = sqlx::query_scalar("select id from secret")
        .fetch_all(env.pool())
        .await?;
    assert_eq!(left, vec![home_secret]);
    assert_eq!(r.json["rows"]["secret"], json!(3), "{}", r.json);
    assert_eq!(r.json["rows"]["quarantine_item"], json!(1), "{}", r.json);
    assert_eq!(
        env.scalar_i64(&format!(
            "select count(*) from quarantine_item where source_id = '{home_mail}'"
        ))
        .await?,
        1
    );
    Ok(())
}
