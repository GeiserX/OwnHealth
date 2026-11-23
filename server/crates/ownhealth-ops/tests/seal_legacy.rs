//! `ownhealth seal-legacy` (`ownhealth_ops::seal_legacy`) on a mixed tree, as an install upgraded from
//! before profile keys holds it: the demo's files sealed under each profile's key, and files a 0.4.0
//! install stored in the plain layout (a report on two profiles, a scan with no profile yet, a held
//! mail, a file an inbox row with no owner names too, a file no row names), plus refresh tokens sealed
//! under the secrets key alone (version 1) and under a profile's key (version 2).
//!
//! The backup test needs pg_dump and pg_restore 17 (OWNHEALTH_PG_BINDIR, see tests/backup.rs).

use std::collections::{BTreeMap, BTreeSet};

use ownhealth_core::blobs::{BlobOwner, BlobStore, tree_files};
use ownhealth_core::config::Settings;
use ownhealth_core::demo::seed_demo;
use ownhealth_core::documents::{NewDocument, create_document};
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
    /// (sha, bytes) of each legacy file
    report: String,
    scan: String,
    held: String,
    shared: String,
    orphan: String,
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
    let mut c = db.pool.acquire().await?;
    fn doc(household: Uuid, profile: Option<Uuid>, sha: &str, n: usize) -> NewDocument<'_> {
        NewDocument::upload(household, profile, sha, "application/pdf", n as i64)
    }
    // the same report on two profiles: each gets its own sealed copy
    create_document(&mut c, &doc(household, Some(alex), &report, REPORT.len())).await?;
    create_document(&mut c, &doc(household, Some(sam), &report, REPORT.len())).await?;
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
    // an inbox row from before migration 1249: no profile, no held mail
    sqlx::query(
        "insert into source_item (source_id, external_id, blob_sha256) values ($1, 'scan-1.pdf', decode($2, 'hex'))",
    )
    .bind(inbox)
    .bind(&shared)
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
        report,
        scan,
        held,
        shared,
        orphan,
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

    /// Every legacy file reads back for its owners, sealed where the job seals it; the plain copies
    /// are gone but for the one an ownerless row names and the one no row names; Alex's token is
    /// sealed under his key and still opens.
    async fn sealed_as_it_should_be(&self) -> R {
        let (alex, sam) = (BlobOwner::Profile(self.alex), BlobOwner::Profile(self.sam));
        let cases = [
            (alex, &self.report, REPORT),
            (sam, &self.report, REPORT),
            (BlobOwner::Pending(self.scan_doc), &self.scan, SCAN),
            (BlobOwner::Pending(self.mail), &self.held, MAIL),
            (alex, &self.shared, SHARED),
        ];
        for (owner, sha, bytes) in cases {
            assert!(self.store.is_sealed(owner, sha)?, "{owner:?} has no sealed copy");
            assert_eq!(self.store.get_bytes(owner, sha)?, bytes, "{owner:?}");
        }
        for sha in [&self.report, &self.scan, &self.held] {
            assert!(
                !self.plain(sha)?,
                "a plain copy stayed after every owner got its sealed one"
            );
        }
        assert!(self.plain(&self.shared)?, "the plain file an ownerless row reads went");
        assert!(self.plain(&self.orphan)?, "a plain file no row names went");
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
    ] {
        assert_eq!(store.get_bytes(owner, sha)?, bytes, "{owner:?}");
    }
    let restored: BTreeSet<String> = tree_files(&fresh.blobs_path())?.into_keys().collect();
    assert!(!restored.contains(&m.report) && restored.contains(&m.shared));
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
