//! `ownhealth seal-legacy` (`ownhealth_ops::seal_legacy`) on a mixed tree, as an install upgraded from
//! before profile keys holds it: the demo's files sealed under each profile's key, and files a 0.4.0
//! install stored in the plain layout (a report on two profiles with its page and OCR layers, a
//! genome's files, a scan with no profile yet, a held mail, inbox rows older than migration 1249: a
//! scanner file with its document, a Health Auto Export push, a released mail, and a household
//! scanner file no document ties to a profile; and a file no row names), plus refresh tokens sealed
//! under the secrets key alone (version 1) and under a profile's key (version 2).
//!
//! The backup test needs pg_dump and pg_restore 17 (OWNHEALTH_PG_BINDIR, see tests/backup.rs).

use std::collections::{BTreeMap, BTreeSet};

use ownhealth_core::blobs::{self, BlobOwner, BlobStore, tree_files};
use ownhealth_core::config::Settings;
use ownhealth_core::demo::seed_demo;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::provenance::{SrcIn, create_src};
use ownhealth_core::quarantine::{QuarantineIn, hold};
use ownhealth_core::secrets::{self, SecretBox, SecretString};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

type R = anyhow::Result<()>;

const REPORT: &[u8] = b"%PDF-1.4 LabNorte report of the Rivera family, stored before the keys";
const SCAN: &[u8] = b"%PDF-1.4 LabNorte scan with no profile yet, stored before the keys";
const MAIL: &[u8] = b"From: lab@labnorte.example\r\nSubject: Resultados\r\n\r\nheld before the keys";
const SHARED: &[u8] = b"%PDF-1.4 LabNorte report that an old inbox row with no owner names too";
const ORPHAN: &[u8] = b"a plain file no row names";
const WORDS: &[u8] = br#"{"words":"page word layer of the LabNorte report, stored before the keys"}"#;
const IMAGE: &[u8] = b"PNG page image of the LabNorte report, stored before the keys";
const OCR: &[u8] = br#"{"words":"OCR layer of the LabNorte report, stored before the keys"}"#;
const FACTS: &[u8] = br#"{"schema":"ownhealth.genome-facts/v1","sample":"RIVERA-SYN-01"}"#;
const VCF: &[u8] = b"##fileformat=VCFv4.2 synthetic genome of Alex, stored before the keys";
const INBOX: &[u8] = b"%PDF-1.4 LabNorte report a scanner folder filed to Alex before 1.0";
const PUSH: &[u8] = br#"{"data":{"metrics":[]},"note":"a Health Auto Export push of Sam before 1.0"}"#;
const EML: &[u8] = b"From: lab@labnorte.example\r\nSubject: Analitica\r\n\r\nreleased to Alex before 1.0";
const CD: &[u8] = b"PK\x03\x04 a LabNorte CD of Alex, zipped in the plain layout";

struct Mixed {
    db: TestDb,
    dir: TempDir,
    settings: Settings,
    store: BlobStore,
    sb: SecretBox,
    alex: Uuid,
    sam: Uuid,
    scan_doc: Uuid,
    mail: Uuid,
    /// the mail released before 1.0
    released: Uuid,
    /// the sha of each legacy file
    report: String,
    scan: String,
    held: String,
    shared: String,
    orphan: String,
    /// a report's page and OCR layers and a genome's files, all Alex's
    layers: Vec<(String, &'static [u8])>,
    inbox: String,
    push: String,
    eml: String,
    /// the version 1 refresh token of Alex's connection, its value, and the other secrets' ids
    token: Uuid,
    token_value: String,
    others: Vec<Uuid>,
}

async fn mixed() -> anyhow::Result<Mixed> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(&dir.path().join("live"))?;
    let store = BlobStore::for_settings(&settings)?;
    assert!(seed_demo(&db.pool, &settings, &store, false).await?);
    let sb = SecretBox::from_settings(&settings)?.with_keys(store.keyring());
    let (alex, household): (Uuid, Uuid) = sqlx::query_as("select id, household_id from profile where slug = 'alex'")
        .fetch_one(&db.pool)
        .await?;
    let sam: Uuid = sqlx::query_scalar("select id from profile where slug = 'sam'")
        .fetch_one(&db.pool)
        .await?;
    // what 0.4.0 wrote: the plain layout, whoever the file was for
    let plain = BlobStore::new(settings.blobs_path())?;
    let put = |b: &[u8]| plain.put_bytes(BlobOwner::Reference, b);
    let (report, scan, held, shared, orphan) = (put(REPORT)?, put(SCAN)?, put(MAIL)?, put(SHARED)?, put(ORPHAN)?);
    let (words, image, ocr, facts, vcf) = (put(WORDS)?, put(IMAGE)?, put(OCR)?, put(FACTS)?, put(VCF)?);
    let (inbox_sha, push, eml) = (put(INBOX)?, put(PUSH)?, put(EML)?);
    let mut c = db.pool.acquire().await?;
    fn doc(household: Uuid, profile: Option<Uuid>, sha: &str, n: usize) -> NewDocument<'_> {
        NewDocument::upload(household, profile, sha, "application/pdf", n as i64)
    }
    // the same report on two profiles: each gets its own sealed copy
    let alex_report = create_document(&mut c, &doc(household, Some(alex), &report, REPORT.len()))
        .await?
        .id;
    create_document(&mut c, &doc(household, Some(sam), &report, REPORT.len())).await?;
    // its page (word layer and image) and its OCR layer, read as the document's
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, words_blob, image_blob) \
         values ($1, 1, 595, 842, true, decode($2, 'hex'), decode($3, 'hex'))",
    )
    .bind(alex_report)
    .bind(&words)
    .bind(&image)
    .execute(&mut *c)
    .await?;
    sqlx::query(
        "insert into ocr_run (document_id, page_no, engine, engine_version, models_sha256, status, words_blob) \
         values ($1, 1, 'paddleocr-onnx', 'test', 'test', 'done', decode($2, 'hex'))",
    )
    .bind(alex_report)
    .bind(&ocr)
    .execute(&mut *c)
    .await?;
    // a genome of Alex's: its facts file and its VCF
    let src = create_src(
        &mut c,
        &SrcIn {
            profile_id: Some(alex),
            ..SrcIn::manual(None)
        },
    )
    .await?;
    sqlx::query(
        "insert into genome_dataset (profile_id, sample_id, assay, facts_schema, facts_sha256, facts_blob, vcf_blob, \
         owngenome_version, db_versions, src_id) values ($1, 'RIVERA-SYN-01', 'wgs', 'ownhealth.genome-facts/v1', \
         decode($2, 'hex'), decode($2, 'hex'), decode($3, 'hex'), '1.4.2', '{}', $4)",
    )
    .bind(alex)
    .bind(&facts)
    .bind(&vcf)
    .bind(&src)
    .execute(&mut *c)
    .await?;
    let scan_doc = create_document(&mut c, &doc(household, None, &scan, SCAN.len()))
        .await?
        .id;
    let mail = Uuid::new_v4();
    hold(
        &mut c,
        &QuarantineIn {
            id: mail,
            household_id: household,
            source_id: None,
            raw_sha256: &held,
            message_id: Some("<legacy@labnorte.example>"),
            sender: Some("lab@labnorte.example"),
            subject: Some("Resultados"),
            attachments: json!([]),
            reason: "sender_not_allowed",
        },
    )
    .await?;
    create_document(&mut c, &doc(household, Some(alex), &shared, SHARED.len())).await?;
    let inbox: Uuid = sqlx::query_scalar(
        "insert into source (household_id, kind, label) values ($1, 'folder', 'Scanner') returning id",
    )
    .bind(household)
    .fetch_one(&mut *c)
    .await?;
    // inbox rows from before migration 1249 name no profile. A household scanner file that no document
    // names: nobody can say whose it is
    sqlx::query(
        "insert into source_item (source_id, external_id, blob_sha256) values ($1, 'scan-1.pdf', decode($2, 'hex'))",
    )
    .bind(inbox)
    .bind(&shared)
    .execute(&mut *c)
    .await?;
    // a scanner file with the document the import made of it, which is Alex's
    let inbox_doc = create_document(&mut c, &doc(household, Some(alex), &inbox_sha, INBOX.len()))
        .await?
        .id;
    sqlx::query(
        "insert into source_item (source_id, external_id, blob_sha256, document_id) \
         values ($1, 'scan-2.pdf', decode($2, 'hex'), $3)",
    )
    .bind(inbox)
    .bind(&inbox_sha)
    .bind(inbox_doc)
    .execute(&mut *c)
    .await?;
    // a Health Auto Export push, on Sam's own inbox
    let hae: Uuid = sqlx::query_scalar(
        "insert into source (household_id, profile_id, kind, label) values ($1, $2, 'health_auto_export', 'Watch') \
         returning id",
    )
    .bind(household)
    .bind(sam)
    .fetch_one(&mut *c)
    .await?;
    sqlx::query("insert into source_item (source_id, external_id, blob_sha256) values ($1, $2, decode($3, 'hex'))")
        .bind(hae)
        .bind(format!("push:{push}"))
        .bind(&push)
        .execute(&mut *c)
        .await?;
    // a mail released into Alex's profile: its whole .eml, named by its item only
    let released = Uuid::new_v4();
    hold(
        &mut c,
        &QuarantineIn {
            id: released,
            household_id: household,
            source_id: None,
            raw_sha256: &eml,
            message_id: Some("<released@labnorte.example>"),
            sender: Some("lab@labnorte.example"),
            subject: Some("Analitica"),
            attachments: json!([]),
            reason: "sender_not_allowed",
        },
    )
    .await?;
    sqlx::query("update quarantine_item set status = 'released', released_document_ids = array[$2] where id = $1")
        .bind(released)
        .bind(alex_report)
        .execute(&mut *c)
        .await?;
    sqlx::query(
        "insert into source_item (source_id, external_id, blob_sha256, quarantine_id) \
         values ($1, '<released@labnorte.example>', decode($2, 'hex'), $3)",
    )
    .bind(inbox)
    .bind(&eml)
    .bind(released)
    .execute(&mut *c)
    .await?;
    // refresh tokens: Alex's under the secrets key alone (version 1), Sam's under his key (version
    // 2), and a household connection's (no profile) under the secrets key, where it stays
    let token_value = format!("refresh-{}", Uuid::new_v4().simple());
    let refresh = "connector_refresh_token";
    let token = secrets::store_secret(&mut c, &sb, household, refresh, None, &SecretString::new(&token_value)).await?;
    let sams =
        secrets::store_profile_secret(&mut c, &sb, household, sam, refresh, &SecretString::new("sam-refresh")).await?;
    let house = secrets::store_secret(&mut c, &sb, household, refresh, None, &SecretString::new("house")).await?;
    for (profile, secret) in [(Some(alex), token), (Some(sam), sams), (None, house)] {
        sqlx::query("insert into source (household_id, profile_id, kind, label, secret_id) values ($1, $2, 'dexcom', 'Dexcom', $3)")
            .bind(household)
            .bind(profile)
            .bind(secret)
            .execute(&mut *c)
            .await?;
    }
    drop(c);
    Ok(Mixed {
        db,
        dir,
        settings,
        store,
        sb,
        alex,
        sam,
        scan_doc,
        mail,
        released,
        report,
        scan,
        held,
        shared,
        orphan,
        layers: vec![(words, WORDS), (image, IMAGE), (ocr, OCR), (facts, FACTS), (vcf, VCF)],
        inbox: inbox_sha,
        push,
        eml,
        token,
        token_value,
        others: vec![sams, house],
    })
}

impl Mixed {
    async fn run(&self, dry_run: bool) -> anyhow::Result<Value> {
        let r = ownhealth_ops::seal_legacy(&self.db.pool, &self.store, &self.sb, dry_run).await?;
        assert_eq!(r["ok"], true, "{r:#}");
        assert_eq!(r["dry_run"], dry_run);
        Ok(r)
    }

    fn keys(&self) -> anyhow::Result<BTreeSet<String>> {
        Ok(tree_files(&self.settings.blobs_path())?.into_keys().collect())
    }

    fn plain(&self, sha: &str) -> anyhow::Result<bool> {
        Ok(self.keys()?.contains(sha))
    }

    async fn ciphertexts(&self) -> anyhow::Result<BTreeMap<Uuid, Vec<u8>>> {
        let rows: Vec<(Uuid, Vec<u8>)> = sqlx::query_as("select id, ciphertext from secret")
            .fetch_all(&self.db.pool)
            .await?;
        Ok(rows.into_iter().collect())
    }

    async fn key_rows(&self) -> anyhow::Result<i64> {
        Ok(
            sqlx::query_scalar("select (select count(*) from profile_key) + (select count(*) from pending_key)")
                .fetch_one(&self.db.pool)
                .await?,
        )
    }

    /// The profile each older inbox row names, by external id.
    async fn item_profiles(&self) -> anyhow::Result<BTreeMap<String, Option<Uuid>>> {
        let rows: Vec<(String, Option<Uuid>)> = sqlx::query_as("select external_id, profile_id from source_item")
            .fetch_all(&self.db.pool)
            .await?;
        Ok(rows.into_iter().collect())
    }

    /// Every legacy file reads back for its owners, sealed where the job seals it; the plain copies
    /// are gone but for the one an ownerless row names and the one no row names; each older inbox row
    /// names the profile its file is for, and the released mail got no key; Alex's token is sealed
    /// under his key and still opens.
    async fn sealed_as_it_should_be(&self) -> R {
        let (alex, sam) = (BlobOwner::Profile(self.alex), BlobOwner::Profile(self.sam));
        let mut cases = vec![
            (alex, &self.report, REPORT),
            (sam, &self.report, REPORT),
            (BlobOwner::Pending(self.scan_doc), &self.scan, SCAN),
            (BlobOwner::Pending(self.mail), &self.held, MAIL),
            (alex, &self.shared, SHARED),
            (alex, &self.inbox, INBOX),
            (sam, &self.push, PUSH),
            (alex, &self.eml, EML),
        ];
        cases.extend(self.layers.iter().map(|(sha, bytes)| (alex, sha, *bytes)));
        for (owner, sha, bytes) in cases {
            assert!(
                self.store.is_sealed(owner, sha)?,
                "{owner:?} has no sealed copy of {sha}"
            );
            assert_eq!(self.store.get_bytes(owner, sha)?, bytes, "{owner:?}");
        }
        let gone = [&self.report, &self.scan, &self.held, &self.inbox, &self.push, &self.eml];
        for sha in gone.into_iter().chain(self.layers.iter().map(|(sha, _)| sha)) {
            assert!(
                !self.plain(sha)?,
                "a plain copy stayed after every owner got its sealed one: {sha}"
            );
        }
        assert!(self.plain(&self.shared)?, "the plain file an ownerless row reads went");
        assert!(self.plain(&self.orphan)?, "a plain file no row names went");
        let items = self.item_profiles().await?;
        assert_eq!(items["scan-1.pdf"], None, "{items:?}");
        assert_eq!(items["scan-2.pdf"], Some(self.alex), "{items:?}");
        assert_eq!(items[&format!("push:{}", self.push)], Some(self.sam), "{items:?}");
        assert_eq!(items["<released@labnorte.example>"], Some(self.alex), "{items:?}");
        // the released mail is read by its profile only: no key of its own, no sealed copy
        assert!(!self.store.is_sealed(BlobOwner::Pending(self.released), &self.eml)?);
        let released_keys: i64 = sqlx::query_scalar("select count(*) from pending_key where item_id = $1")
            .bind(self.released)
            .fetch_one(&self.db.pool)
            .await?;
        assert_eq!(released_keys, 0, "the released mail got a key of its own");
        let mut c = self.db.pool.acquire().await?;
        assert!(secrets::sealed_for_profile(&mut c, self.token, self.alex).await?);
        let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
            .bind(self.alex)
            .fetch_one(&mut *c)
            .await?;
        let read = secrets::read_secret(&mut c, &self.sb, household, self.token).await?;
        assert_eq!(read.expose(), self.token_value);
        Ok(())
    }
}

fn counts(r: &Value) -> Value {
    json!([r["files"], r["tokens"]])
}

#[tokio::test]
async fn the_job_seals_a_mixed_tree_and_a_second_run_changes_nothing() -> R {
    let m = mixed().await?;
    let before = m.ciphertexts().await?;
    let first = m.run(false).await?;
    assert!(first["files"]["sealed"].as_u64() >= Some(5), "{first:#}");
    assert!(first["files"]["removed"].as_u64() >= Some(3), "{first:#}");
    assert!(first["files"]["kept"]["ownerless"].as_u64() >= Some(1), "{first:#}");
    assert!(first["files"]["kept"]["unnamed"].as_u64() >= Some(1), "{first:#}");
    assert_eq!(first["tokens"], json!({"version_1": 1, "sealed": 1, "failed": 0}));
    // the files that stay are named, by why
    assert_eq!(
        first["files"]["kept_files"]["ownerless"],
        json!([m.shared]),
        "{first:#}"
    );
    let unnamed = first["files"]["kept_files"]["unnamed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(unnamed.contains(&json!(m.orphan)), "{first:#}");
    m.sealed_as_it_should_be().await?;
    // only Alex's version 1 token changed: Sam's version 2 and the household's stay as they were
    let after = m.ciphertexts().await?;
    for id in &m.others {
        assert_eq!(before[id], after[id]);
    }
    assert_ne!(before[&m.token], after[&m.token]);
    let (keys, secrets) = (m.keys()?, after);
    let second = m.run(false).await?;
    assert_eq!(second["files"]["sealed"], 0, "{second:#}");
    assert_eq!(second["files"]["removed"], 0, "{second:#}");
    let left = first["files"]["plain"].as_u64().unwrap_or(0) - first["files"]["removed"].as_u64().unwrap_or(0);
    assert_eq!(second["files"]["plain"], left, "{second:#}");
    assert_eq!(second["tokens"], json!({"version_1": 0, "sealed": 0, "failed": 0}));
    assert_eq!(m.keys()?, keys, "the second run changed the tree");
    assert_eq!(m.ciphertexts().await?, secrets, "the second run changed a secret");
    m.sealed_as_it_should_be().await?;
    Ok(())
}

#[tokio::test]
async fn a_dry_run_counts_what_a_run_does_and_changes_nothing() -> R {
    let m = mixed().await?;
    let (keys, secrets, key_rows) = (m.keys()?, m.ciphertexts().await?, m.key_rows().await?);
    let dry = m.run(true).await?;
    assert!(dry["files"]["sealed"].as_u64() >= Some(5), "{dry:#}");
    assert!(dry["files"]["removed"].as_u64() >= Some(3), "{dry:#}");
    assert_eq!(dry["tokens"]["sealed"], 1, "{dry:#}");
    assert_eq!(m.keys()?, keys, "a dry run changed the tree");
    assert_eq!(m.ciphertexts().await?, secrets, "a dry run changed a secret");
    assert_eq!(m.key_rows().await?, key_rows, "a dry run made a key");
    let real = m.run(false).await?;
    assert_eq!(counts(&dry), counts(&real));
    Ok(())
}

/// A run stopped halfway leaves files in any of the states one file goes through: sealed for some of
/// its owners with the plain copy still there, sealed for all with the plain copy there, done, or not
/// touched, plus a half-written seal in the store's tmp folder. The next run finishes every file.
#[tokio::test]
async fn a_run_stopped_halfway_is_finished_by_the_next() -> R {
    let m = mixed().await?;
    // the report sealed for Alex only, the scan sealed with its plain copy left, the held mail done
    m.store
        .rekey(BlobOwner::Reference, BlobOwner::Profile(m.alex), &m.report)?;
    m.store
        .rekey(BlobOwner::Reference, BlobOwner::Pending(m.scan_doc), &m.scan)?;
    m.store
        .rekey(BlobOwner::Reference, BlobOwner::Pending(m.mail), &m.held)?;
    assert!(m.store.drop_plain(&m.held)?);
    std::fs::write(m.settings.blobs_path().join("tmp").join("0".repeat(32)), b"half a seal")?;
    let r = m.run(false).await?;
    assert_eq!(r["tokens"]["sealed"], 1, "{r:#}");
    m.sealed_as_it_should_be().await?;
    let again = m.run(false).await?;
    assert_eq!(
        (again["files"]["sealed"].clone(), again["files"]["removed"].clone()),
        (json!(0), json!(0))
    );
    Ok(())
}

/// After the job, a backup holds the sealed copies, and verify-restore and restore pass: every file
/// opens for its owners on the restored instance, and the plain file an ownerless row names is there.
#[tokio::test]
async fn backup_verify_restore_and_restore_pass_after_the_job() -> R {
    let m = mixed().await?;
    m.run(false).await?;
    let archive = ownhealth_ops::backup(&m.settings, &m.dir.path().join("backup").join("staging")).await?;
    let report = ownhealth_ops::verify_restore(&m.settings, &archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    let fresh_db = TestDb::empty().await?;
    let mut fresh = fresh_db.settings(&m.dir.path().join("fresh"))?;
    fresh.secrets_key = m.settings.secrets_key.clone();
    // a restore on the same host reads the live keyring (slice 47): the backup holds no profile key
    fresh.keyring_dir = Some(m.settings.keyring_path());
    let report = ownhealth_ops::restore(&fresh, &archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    let store = BlobStore::for_settings(&fresh)?;
    for (owner, sha, bytes) in [
        (BlobOwner::Profile(m.alex), &m.report, REPORT),
        (BlobOwner::Profile(m.sam), &m.report, REPORT),
        (BlobOwner::Pending(m.scan_doc), &m.scan, SCAN),
        (BlobOwner::Pending(m.mail), &m.held, MAIL),
        (BlobOwner::Profile(m.alex), &m.shared, SHARED),
        (BlobOwner::Reference, &m.shared, SHARED),
        (BlobOwner::Profile(m.alex), &m.inbox, INBOX),
        (BlobOwner::Profile(m.sam), &m.push, PUSH),
        (BlobOwner::Profile(m.alex), &m.eml, EML),
    ] {
        assert_eq!(store.get_bytes(owner, sha)?, bytes, "{owner:?}");
    }
    let restored: BTreeSet<String> = tree_files(&fresh.blobs_path())?.into_keys().collect();
    assert!(!restored.contains(&m.report) && restored.contains(&m.shared));
    assert!(!restored.contains(&m.inbox) && !restored.contains(&m.push) && !restored.contains(&m.eml));
    Ok(())
}

/// A backup that copies the files holds the copy lock: the job waits for it before it removes a
/// plain file the backup's snapshot names, and goes on once the copy is done.
#[tokio::test]
async fn a_run_waits_for_a_backup_that_copies_the_files() -> R {
    let m = mixed().await?;
    let mut backup = m.db.pool.acquire().await?;
    blobs::hold_for_copy(&mut backup).await?;
    let (pool, store, sb) = (m.db.pool.clone(), m.store.clone(), m.sb.clone());
    let run = tokio::spawn(async move { ownhealth_ops::seal_legacy(&pool, &store, &sb, false).await });
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!run.is_finished(), "the job did not wait for the backup");
    for sha in [&m.report, &m.scan, &m.held, &m.inbox] {
        assert!(m.plain(sha)?, "a plain file went while a backup copied the files");
    }
    blobs::end_copy(&mut backup).await?;
    let r = run.await??;
    assert_eq!(r["ok"], true, "{r:#}");
    m.sealed_as_it_should_be().await?;
    Ok(())
}

/// A plain file whose bytes no longer match its sha256 is not sealed and stays as it was; the run
/// reports it by its sha256 and its document, says ok false, and goes on with the other files.
#[tokio::test]
async fn a_corrupt_plain_file_fails_the_run_and_stays() -> R {
    use std::os::unix::fs::PermissionsExt;
    let m = mixed().await?;
    let path = m
        .settings
        .blobs_path()
        .join("sha256")
        .join(&m.inbox[..2])
        .join(&m.inbox[2..4])
        .join(&m.inbox);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
    let mut bytes = INBOX.to_vec();
    bytes[10] ^= 1;
    std::fs::write(&path, &bytes)?;
    let r = ownhealth_ops::seal_legacy(&m.db.pool, &m.store, &m.sb, false).await?;
    assert_eq!(r["ok"], false, "{r:#}");
    assert_eq!(r["files"]["failed"], 1, "{r:#}");
    let errors = r["errors"].to_string();
    assert!(errors.contains(&m.inbox), "the error does not name the file: {errors}");
    let doc: Uuid = sqlx::query_scalar("select id from document where blob_sha256 = decode($1, 'hex')")
        .bind(&m.inbox)
        .fetch_one(&m.db.pool)
        .await?;
    assert!(
        errors.contains(&doc.to_string()),
        "the error does not name the document: {errors}"
    );
    assert_eq!(std::fs::read(&path)?, bytes, "the corrupt plain file changed");
    assert!(!m.store.is_sealed(BlobOwner::Profile(m.alex), &m.inbox)?);
    // the other files were sealed
    assert!(m.store.is_sealed(BlobOwner::Profile(m.sam), &m.push)?);
    assert!(!m.plain(&m.push)?);
    Ok(())
}

#[tokio::test]
async fn an_inbox_claim_uses_the_linked_owner_when_the_document_names_different_bytes() -> R {
    let m = mixed().await?;
    sqlx::query(
        "update source_item set document_id = (select id from document where profile_id = $1 \
         and blob_sha256 = decode($2, 'hex')) where external_id = 'scan-2.pdf'",
    )
    .bind(m.sam)
    .bind(&m.report)
    .execute(&m.db.pool)
    .await?;
    m.run(false).await?;
    assert_eq!(m.item_profiles().await?["scan-2.pdf"], Some(m.sam));
    assert!(m.store.is_sealed(BlobOwner::Profile(m.sam), &m.inbox)?);
    assert!(!m.plain(&m.inbox)?);
    Ok(())
}

#[tokio::test]
async fn the_report_counts_a_sealed_copy_when_a_later_owner_fails() -> R {
    let m = mixed().await?;
    for sha in m.store.plain_files()? {
        if sha != m.report {
            m.store.drop_plain(&sha)?;
        }
    }
    let owners: Vec<Uuid> =
        sqlx::query_scalar("select profile_id from document where blob_sha256 = decode($1, 'hex') order by id")
            .bind(&m.report)
            .fetch_all(&m.db.pool)
            .await?;
    let blocked = m.settings.blobs_path().join("enc").join(owners[1].simple().to_string());
    if blocked.exists() {
        std::fs::rename(&blocked, m.dir.path().join("saved-slot"))?;
    }
    std::fs::write(&blocked, b"block the second owner's sealed folder")?;

    let report = ownhealth_ops::seal_legacy(&m.db.pool, &m.store, &m.sb, false).await?;
    assert!(m.store.is_sealed(BlobOwner::Profile(owners[0]), &m.report)?);
    assert!(m.plain(&m.report)?);
    assert_eq!(report["files"]["sealed"], 1, "{report:#}");
    assert_eq!(report["files"]["removed"], 0, "{report:#}");
    assert_eq!(report["files"]["failed"], 1, "{report:#}");
    assert!(
        report["errors"].to_string().contains("retained for retry"),
        "{report:#}"
    );
    Ok(())
}

#[tokio::test]
async fn key_preflight_tries_a_valid_secret_after_a_corrupt_match() -> R {
    let m = mixed().await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select id from secret where key_fingerprint = $1 and substring(ciphertext from 1 for 1) = '\\x01'::bytea order by id",
    )
    .bind(&m.sb.fingerprint()[..])
    .fetch_all(&m.db.pool)
    .await?;
    assert!(ids.len() >= 2);
    sqlx::query("update secret set ciphertext = '\\x0100'::bytea where id = $1")
        .bind(ids[0])
        .execute(&m.db.pool)
        .await?;
    let preview = m.run(true).await?;
    assert!(
        preview["files"]["removed"].as_u64().expect("removed is a count") > 0,
        "{preview:#}"
    );
    let report = ownhealth_ops::seal_legacy(&m.db.pool, &m.store, &m.sb, false).await?;
    assert!(m.store.is_sealed(BlobOwner::Profile(m.alex), &m.report)?);
    assert_eq!(report["tokens"]["failed"], usize::from(ids[0] == m.token), "{report:#}");
    Ok(())
}

/// A run with a secrets key other than the install's refuses before it writes, in a dry run too,
/// even on an install upgraded from before 1.0 that holds no profile key yet: its keys would open
/// only under the other key, and the plain copies would be gone.
#[tokio::test]
async fn a_run_with_another_secrets_key_refuses_and_changes_nothing() -> R {
    let m = mixed().await?;
    // as 0.4.0 left it: no profile key and no pending key yet
    sqlx::query("delete from pending_key").execute(&m.db.pool).await?;
    sqlx::query("delete from profile_key").execute(&m.db.pool).await?;
    let mut other = m.settings.clone();
    other.secrets_key = Some(ownhealth_core::testing::random_secrets_key());
    let store = BlobStore::for_settings(&other)?;
    let sb = SecretBox::from_settings(&other)?.with_keys(store.keyring());
    let (keys, secrets, key_rows) = (m.keys()?, m.ciphertexts().await?, m.key_rows().await?);
    for dry_run in [true, false] {
        let err = ownhealth_ops::seal_legacy(&m.db.pool, &store, &sb, dry_run)
            .await
            .expect_err("a run with another key went ahead");
        assert!(err.to_string().contains("OWNHEALTH_SECRETS_KEY"), "{err}");
    }
    assert_eq!(m.keys()?, keys, "the tree changed");
    assert_eq!(m.ciphertexts().await?, secrets, "a secret changed");
    assert_eq!(m.key_rows().await?, key_rows, "a key was made");
    Ok(())
}

/// A reader that took a plain file as a path (`with_local_copy`: SQLite, a zip) keeps reading it
/// while the job seals the file and removes the plain copy.
#[tokio::test]
async fn a_local_copy_of_a_plain_file_outlives_the_job() -> R {
    let m = mixed().await?;
    let copy = m.store.with_local_copy(BlobOwner::Profile(m.alex), &m.report)?;
    m.run(false).await?;
    assert!(!m.plain(&m.report)?, "the plain copy stayed");
    assert_eq!(std::fs::read(copy.path())?, REPORT);
    let path = copy.path().to_path_buf();
    drop(copy);
    assert!(!path.exists(), "the copy stayed after its reader");
    Ok(())
}

/// An imaging upload in the plain layout (the zip an `imaging_import` row names) is sealed under its
/// profile's key, and the plain copy goes.
#[tokio::test]
async fn an_imaging_upload_is_sealed_under_its_profile() -> R {
    let m = mixed().await?;
    let zip = BlobStore::new(m.settings.blobs_path())?.put_bytes(BlobOwner::Reference, CD)?;
    sqlx::query(
        "insert into imaging_import (profile_id, zip_blob, filename, size_bytes, files) \
         values ($1, decode($2, 'hex'), 'cd.zip', $3, 1)",
    )
    .bind(m.alex)
    .bind(&zip)
    .bind(i64::try_from(CD.len())?)
    .execute(&m.db.pool)
    .await?;
    m.run(false).await?;
    let alex = BlobOwner::Profile(m.alex);
    assert!(
        m.store.is_sealed(alex, &zip)?,
        "the CD zip was not sealed for its profile"
    );
    assert_eq!(m.store.get_bytes(alex, &zip)?, CD);
    assert!(!m.plain(&zip)?, "the plain CD zip stayed");
    Ok(())
}

/// The job reads the owners of every blob column profile delete knows: a new blob column must join
/// both, or its plain files would stay (or, read by another owner, be removed under a reader).
#[test]
fn the_job_reads_every_blob_column() {
    use ownhealth_auth::household::{BLOB_COLUMNS, HOUSEHOLD_BLOB_COLUMNS};
    let ours: BTreeSet<(String, String)> = ownhealth_ops::seal::COLUMNS
        .iter()
        .flat_map(|(t, cols)| cols.iter().map(|c| ((*t).to_string(), (*c).to_string())))
        .collect();
    let all: BTreeSet<(String, String)> = BLOB_COLUMNS
        .iter()
        .flat_map(|(t, cols, _)| cols.iter().map(|c| ((*t).to_string(), (*c).to_string())))
        .chain(
            HOUSEHOLD_BLOB_COLUMNS
                .iter()
                .map(|(t, c)| ((*t).to_string(), (*c).to_string())),
        )
        .collect();
    assert_eq!(ours, all);
}

/// The removal check of an upload (`ownhealth_core::blobs::HOLDERS`) counts every blob column the job
/// counts, which is every blob column a backup lists: a column it leaves out would let the removal of
/// an upload delete bytes that row still names.
#[test]
fn the_upload_removal_check_counts_every_blob_column() {
    let ours: BTreeSet<(String, String)> = ownhealth_ops::seal::COLUMNS
        .iter()
        .flat_map(|(t, cols)| cols.iter().map(|c| ((*t).to_string(), (*c).to_string())))
        .collect();
    let removal: BTreeSet<(String, String)> = ownhealth_core::blobs::HOLDERS
        .iter()
        .flat_map(|(t, cols, _, _)| cols.iter().map(|c| ((*t).to_string(), (*c).to_string())))
        .collect();
    assert_eq!(removal, ours);
}
