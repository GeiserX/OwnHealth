//! The audit chain head kept outside the database (backup.rs): every backup records the head in its
//! manifest and in the anchor file, and refuses to run when the head an earlier backup anchored is no
//! longer in the live chain. That is the one owner rewrite `verify_chain` cannot see by itself: an
//! edited row with the whole chain hashed again from there, or the newest rows removed with their
//! links.
//!
//! pg_dump and pg_restore 17 come from PATH or OWNHEALTH_PG_BINDIR, as for tests/backup.rs.

use std::fs;
use std::path::{Path, PathBuf};

use ownhealth_core::audit::{AuditEvent, verify_chain, write_audit};
use ownhealth_core::config::Settings;
use ownhealth_core::testing::TestDb;
use serde_json::Value;
use tempfile::TempDir;

type R = anyhow::Result<()>;

/// A migrated database with a few audit events, and its settings (data under a temp dir).
struct Live {
    db: TestDb,
    settings: Settings,
    dir: TempDir,
}

async fn live() -> anyhow::Result<Live> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(&dir.path().join("live"))?;
    fs::create_dir_all(settings.blobs_path())?;
    let l = Live { db, settings, dir };
    add_events(&l, 3).await?;
    Ok(l)
}

async fn add_events(l: &Live, n: usize) -> R {
    let mut c = l.db.pool.acquire().await?;
    for i in 0..n {
        let e = AuditEvent {
            tool: Some(format!("GET /api/test/{i}")),
            ..AuditEvent::new("system", "job", "read")
        };
        write_audit(&mut c, &e).await?;
    }
    Ok(())
}

impl Live {
    fn dest(&self, name: &str) -> PathBuf {
        self.dir.path().join("backups").join(name)
    }

    fn anchor_file(&self) -> PathBuf {
        self.settings.anchor_path().join("audit-head.json")
    }

    async fn backup(&self, name: &str) -> ownhealth_core::Result<PathBuf> {
        ownhealth_ops::backup(&self.settings, &self.dest(name)).await
    }

    /// The newest link: (seq, hash as hex).
    async fn head(&self) -> anyhow::Result<(i64, String)> {
        let (seq, hash): (i64, Vec<u8>) = sqlx::query_as("select seq, hash from audit_chain order by seq desc limit 1")
            .fetch_one(&self.db.pool)
            .await?;
        Ok((seq, hex::encode(hash)))
    }

    async fn status(&self) -> anyhow::Result<Value> {
        let mut c = self.db.pool.acquire().await?;
        Ok(ownhealth_ops::audit_status(&self.settings, &mut c).await?)
    }

    /// As the database owner: edit the oldest event, then hash the whole chain again from seq 1, so
    /// every link holds and `verify_chain` passes.
    async fn rewrite_and_rehash(&self) -> R {
        sqlx::raw_sql(
            "alter table audit_event disable trigger user; alter table audit_chain disable trigger user; \
             update audit_event set tool = 'rewritten' where id = (select min(id) from audit_event); \
             do $$ declare c record; prev bytea := decode(repeat('00', 32), 'hex'); h bytea; begin \
               for c in select seq, event_id from audit_chain order by seq loop \
                 select sha256(prev || oh_audit_row_v1(e)) into h from audit_event e where e.id = c.event_id; \
                 update audit_chain set prev_hash = prev, hash = h where seq = c.seq; \
                 prev := h; \
               end loop; end $$; \
             alter table audit_event enable trigger user; alter table audit_chain enable trigger user",
        )
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    /// As the database owner: remove the newest event and its link. The shorter chain still holds.
    async fn drop_newest(&self) -> R {
        sqlx::raw_sql(
            "alter table audit_event disable trigger user; alter table audit_chain disable trigger user; \
             delete from audit_event where id = (select event_id from audit_chain order by seq desc limit 1); \
             delete from audit_chain where seq = (select max(seq) from audit_chain); \
             alter table audit_event enable trigger user; alter table audit_chain enable trigger user",
        )
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    /// As the database owner: edit one event and leave its link alone.
    async fn edit_without_rehash(&self) -> anyhow::Result<i64> {
        sqlx::raw_sql(
            "alter table audit_event disable trigger user; \
             update audit_event set tool = 'edited' where id = (select max(id) from audit_event); \
             alter table audit_event enable trigger user",
        )
        .execute(&self.db.pool)
        .await?;
        Ok(sqlx::query_scalar("select max(seq) from audit_chain")
            .fetch_one(&self.db.pool)
            .await?)
    }
}

fn manifest(dir: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?)
}

fn anchor(l: &Live) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&fs::read(l.anchor_file())?)?)
}

fn err_text<T>(r: ownhealth_core::Result<T>) -> String {
    r.err().map(|e| e.to_string()).unwrap_or_default()
}

#[cfg(unix)]
fn mode(p: &Path) -> anyhow::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(p)?.permissions().mode() & 0o777)
}

#[tokio::test]
async fn the_first_backup_records_the_head_and_anchors_it_in_an_owner_only_file() -> R {
    let l = live().await?;
    assert_eq!(
        l.status().await?["anchor"]["state"],
        "none",
        "no backup has anchored yet"
    );
    let (seq, hash) = l.head().await?;
    let dest = l.backup("b1").await?;
    let m = manifest(&dest)?;
    assert_eq!(m["audit_chain"]["seq"], seq, "{m:#}");
    assert_eq!(m["audit_chain"]["hash"], hash.as_str());
    assert_eq!(m["audit_chain"]["verified"], seq);
    assert_eq!(m["audit_chain"]["broken"], Value::Null);
    let a = anchor(&l)?;
    assert_eq!(a["schema"], "ownhealth.audit-anchor/v1");
    assert_eq!(
        (a["seq"].clone(), a["hash"].clone()),
        (Value::from(seq), Value::from(hash))
    );
    assert_eq!(a["at"], m["created_at"]);
    #[cfg(unix)]
    {
        assert_eq!(mode(&l.anchor_file())?, 0o600);
        assert_eq!(mode(&l.settings.anchor_path())?, 0o700);
    }
    // written through a temporary file and a rename: nothing else is left beside it
    let names: Vec<String> = fs::read_dir(l.settings.anchor_path())?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(names, ["audit-head.json"]);
    let s = l.status().await?;
    assert_eq!(
        (s["ok"].clone(), s["anchor"]["state"].clone()),
        (Value::Bool(true), Value::from("ok")),
        "{s:#}"
    );
    assert_eq!(s["anchor"]["seq"], seq);
    assert_eq!(s["verified"], seq);
    Ok(())
}

#[tokio::test]
async fn a_log_that_only_grew_still_matches_its_anchor() -> R {
    let l = live().await?;
    l.backup("b1").await?;
    add_events(&l, 2).await?;
    let (seq, hash) = l.head().await?;
    l.backup("b2").await?;
    let a = anchor(&l)?;
    assert_eq!(
        (a["seq"].clone(), a["hash"].clone()),
        (Value::from(seq), Value::from(hash))
    );
    Ok(())
}

/// The attack the anchor exists for: the chain is consistent again, so `verify_chain` passes, but
/// the next backup refuses loudly, leaves the anchor as it was, and the page shows the mismatch.
#[tokio::test]
async fn a_rewritten_and_rehashed_log_passes_verify_but_fails_the_next_backup() -> R {
    let l = live().await?;
    l.backup("b1").await?;
    let before = fs::read(l.anchor_file())?;
    let (seq, _) = l.head().await?;
    l.rewrite_and_rehash().await?;
    let check = verify_chain(&mut *l.db.pool.acquire().await?).await?;
    assert!(
        check.ok() && check.verified == seq,
        "the rehashed chain holds: {check:?}"
    );

    let err = err_text(l.backup("b2").await);
    assert!(
        err.contains("no longer matches") && err.contains(&format!("entry {seq}")),
        "{err}"
    );
    assert!(
        err.contains("audit-head.json"),
        "the message names the file to move aside: {err}"
    );
    assert_eq!(
        fs::read(l.anchor_file())?,
        before,
        "a refused backup leaves the anchor alone"
    );
    assert!(!l.dest("b2").exists(), "a refused backup makes no destination folder");

    let s = l.status().await?;
    assert_eq!(s["ok"], false, "{s:#}");
    assert_eq!(s["anchor"]["state"], "mismatch");
    assert_eq!(s["anchor"]["seq"], seq);
    assert_eq!(s["broken"], Value::Null, "the chain itself holds");
    Ok(())
}

/// The newest entries removed together with their links leave a shorter chain that holds; the
/// anchored entry is gone, so the next backup refuses.
#[tokio::test]
async fn removing_the_newest_entries_fails_the_next_backup() -> R {
    let l = live().await?;
    l.backup("b1").await?;
    let (seq, _) = l.head().await?;
    l.drop_newest().await?;
    assert!(verify_chain(&mut *l.db.pool.acquire().await?).await?.ok());
    let err = err_text(l.backup("b2").await);
    assert!(
        err.contains("no longer matches") && err.contains(&format!("entry {seq}")),
        "{err}"
    );
    assert_eq!(l.status().await?["anchor"]["state"], "mismatch");
    Ok(())
}

/// The way out: an operator who has looked moves the anchor aside (kept as evidence), and the next
/// backup anchors the log as it is now.
#[tokio::test]
async fn moving_the_anchor_aside_lets_the_next_backup_anchor_again() -> R {
    let l = live().await?;
    l.backup("b1").await?;
    l.rewrite_and_rehash().await?;
    assert!(l.backup("b2").await.is_err());
    fs::rename(
        l.anchor_file(),
        l.settings.anchor_path().join("audit-head.evidence.json"),
    )?;
    l.backup("b3").await?;
    let (seq, hash) = l.head().await?;
    let a = anchor(&l)?;
    assert_eq!(
        (a["seq"].clone(), a["hash"].clone()),
        (Value::from(seq), Value::from(hash))
    );
    assert_eq!(l.status().await?["anchor"]["state"], "ok");
    Ok(())
}

#[tokio::test]
async fn an_unreadable_anchor_refuses_the_backup_and_shows_on_the_page() -> R {
    let l = live().await?;
    l.backup("b1").await?;
    fs::write(l.anchor_file(), b"{not json")?;
    let err = err_text(l.backup("b2").await);
    assert!(err.contains("cannot read the audit anchor"), "{err}");
    let s = l.status().await?;
    assert_eq!(
        (s["ok"].clone(), s["anchor"]["state"].clone()),
        (Value::Bool(false), Value::from("unreadable"))
    );
    Ok(())
}

/// An edit without the rehash: `verify_chain` names the link, the page shows it as broken, and the
/// backup still runs (a broken link stays broken for good, so refusing would stop every backup of the
/// health data) with the break written into its manifest.
#[tokio::test]
async fn an_edited_row_shows_as_broken_and_the_manifest_records_it() -> R {
    let l = live().await?;
    let seq = l.edit_without_rehash().await?;
    let s = l.status().await?;
    assert_eq!(s["ok"], false, "{s:#}");
    assert_eq!(s["broken"]["seq"], seq);
    assert_eq!(s["broken"]["kind"], "hash");
    assert_eq!(s["verified"], seq - 1);
    let dest = l.backup("b1").await?;
    let m = manifest(&dest)?;
    assert_eq!(m["audit_chain"]["broken"]["seq"], seq, "{m:#}");
    assert_eq!(m["audit_chain"]["verified"], seq - 1);
    assert!(!l.anchor_file().exists(), "a broken log is never anchored");
    Ok(())
}

/// A break after the last anchor: the backup goes on, but the anchor stays where it was, so the
/// break cannot carry a rewrite past it.
#[tokio::test]
async fn a_broken_log_keeps_the_previous_anchor() -> R {
    let l = live().await?;
    l.backup("b1").await?;
    let before = fs::read(l.anchor_file())?;
    add_events(&l, 2).await?;
    l.edit_without_rehash().await?;
    l.backup("b2").await?;
    assert_eq!(
        fs::read(l.anchor_file())?,
        before,
        "the anchor did not move onto a broken log"
    );
    assert_eq!(l.status().await?["anchor"]["state"], "ok");
    Ok(())
}

/// The decoy (review PR 75, H1): the owner rewrites the oldest entry and hashes the chain again, then
/// drops the key and plants a second link with the anchored seq and the anchored hash. The anchor
/// check must not read the decoy, and the backup must not move the anchor onto the rehashed head;
/// once the decoy is gone again, the rewrite still shows.
#[tokio::test]
async fn a_decoy_link_at_the_anchored_entry_does_not_launder_a_rewrite() -> R {
    let l = live().await?;
    l.backup("b1").await?;
    let before = fs::read(l.anchor_file())?;
    let (seq, hash) = l.head().await?;
    add_events(&l, 2).await?;
    l.rewrite_and_rehash().await?;
    // the update puts the real link after the decoy in the table, so a plain read finds the decoy first
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "alter table audit_chain disable trigger user; \
         alter table audit_chain drop constraint audit_chain_pkey; \
         insert into audit_chain (seq, event_id, prev_hash, hash) \
           values ({seq}, -1, decode(repeat('00', 32), 'hex'), decode('{hash}', 'hex')); \
         update audit_chain set hash = hash where seq = {seq} and event_id > 0; \
         alter table audit_chain enable trigger user"
    )))
    .execute(&l.db.pool)
    .await?;
    let s = l.status().await?;
    assert_eq!(s["ok"], false, "{s:#}");
    assert_ne!(
        s["anchor"]["state"], "ok",
        "the decoy passed for the anchored entry: {s:#}"
    );

    // the health data is still backed up, the manifest says the log could not be checked, and the
    // anchor stays where it was
    let dest = l.backup("b2").await?;
    assert_eq!(manifest(&dest)?["audit_chain"]["broken"]["kind"], "uncheckable");
    assert_eq!(fs::read(l.anchor_file())?, before, "the anchor moved past the rewrite");

    // the owner takes the decoy away and puts the key back: the rehashed log holds on its own, and
    // the anchor still catches it
    sqlx::raw_sql(
        "alter table audit_chain disable trigger user; delete from audit_chain where event_id = -1; \
         alter table audit_chain add primary key (seq); alter table audit_chain enable trigger user",
    )
    .execute(&l.db.pool)
    .await?;
    assert!(verify_chain(&mut *l.db.pool.acquire().await?).await?.ok());
    assert_eq!(l.status().await?["anchor"]["state"], "mismatch");
    let err = err_text(l.backup("b3").await);
    assert!(err.contains("no longer matches"), "{err}");
    Ok(())
}

/// An uncheckable log (row security switched on) is treated like a broken one: the backup still runs
/// (audit damage never stops the health backups), the manifest records it, the anchor stays, and
/// the page says the anchor cannot be compared.
#[tokio::test]
async fn an_uncheckable_log_does_not_stop_the_backup() -> R {
    let l = live().await?;
    l.backup("b1").await?;
    let before = fs::read(l.anchor_file())?;
    add_events(&l, 1).await?;
    sqlx::raw_sql("alter table audit_chain enable row level security")
        .execute(&l.db.pool)
        .await?;
    let dest = l.backup("b2").await?;
    let m = manifest(&dest)?;
    assert_eq!(m["audit_chain"]["broken"]["kind"], "uncheckable", "{m:#}");
    assert_eq!(fs::read(l.anchor_file())?, before);
    let s = l.status().await?;
    assert_eq!(
        (
            s["ok"].clone(),
            s["broken"]["kind"].clone(),
            s["anchor"]["state"].clone()
        ),
        (Value::Bool(false), Value::from("uncheckable"), Value::from("unchecked")),
        "{s:#}"
    );
    Ok(())
}

/// Two backups at once (CodeRabbit on PR 75): one reads and replaces the anchor at a time, so a
/// backup that began with an older head cannot finish last and put that head over a newer one. A
/// backup that starts while another holds the anchor waits, then anchors the head it sees.
#[tokio::test]
async fn a_backup_waits_for_the_one_that_holds_the_anchor() -> R {
    use sqlx::Connection;
    let l = live().await?;
    l.backup("b1").await?;
    let mut other = sqlx::PgConnection::connect(&l.db.url).await?;
    sqlx::query("select pg_advisory_lock(hashtext('ownhealth.audit_anchor'))")
        .execute(&mut other)
        .await?;
    let (settings, dest) = (l.settings.clone(), l.dest("b2"));
    let waiting = tokio::spawn(async move { ownhealth_ops::backup(&settings, &dest).await });
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!waiting.is_finished(), "the second backup did not wait");
    add_events(&l, 2).await?; // while it waits: the head it anchors is the newer one
    sqlx::query("select pg_advisory_unlock(hashtext('ownhealth.audit_anchor'))")
        .execute(&mut other)
        .await?;
    waiting.await??;
    let (seq, hash) = l.head().await?;
    let a = anchor(&l)?;
    assert_eq!(
        (a["seq"].clone(), a["hash"].clone()),
        (Value::from(seq), Value::from(hash))
    );
    other.close().await?;
    Ok(())
}

/// The anchor folder and file belong to the backup's user alone. A symlinked folder is never
/// followed (nor its target's mode changed), and a folder or file others may write is refused.
#[cfg(unix)]
#[tokio::test]
async fn an_anchor_folder_others_may_write_is_refused() -> R {
    use std::os::unix::fs::PermissionsExt;
    let mut l = live().await?;
    let shared = l.dir.path().join("shared");
    fs::create_dir_all(&shared)?;
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755))?;
    let link = l.dir.path().join("anchor-link");
    std::os::unix::fs::symlink(&shared, &link)?;
    l.settings.anchor_dir = Some(link);
    let err = err_text(l.backup("b1").await);
    assert!(err.contains("the anchor folder is a symlink"), "{err}");
    assert_eq!(mode(&shared)?, 0o755, "the target of the symlink was changed");
    assert_eq!(l.status().await?["anchor"]["state"], "unreadable");

    l.settings.anchor_dir = Some(shared.clone());
    l.backup("b2").await?;
    assert_eq!(mode(&shared)?, 0o755, "a folder that was there keeps its mode");
    for (path, m, why) in [
        (shared.clone(), 0o777, "the anchor folder is world-writable"),
        (shared.clone(), 0o775, "the anchor folder is group-writable"),
        (
            shared.join("audit-head.json"),
            0o666,
            "the anchor file is world-writable",
        ),
    ] {
        let was = mode(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(m))?;
        let err = err_text(l.backup(&format!("b-{m:o}")).await);
        assert!(err.contains(why), "{why}: {err}");
        assert_eq!(l.status().await?["anchor"]["state"], "unreadable", "{why}");
        fs::set_permissions(&path, fs::Permissions::from_mode(was))?;
    }
    Ok(())
}

/// A normal restore of an older backup (review PR 75, F2): the restored log ends at that backup's
/// head, so the restore sets the anchor there, and the page does not read as a rewrite.
#[tokio::test]
async fn a_restore_anchors_the_head_it_restored() -> R {
    let l = live().await?;
    let older = l.backup("b1").await?;
    let (seq, hash) = l.head().await?;
    add_events(&l, 2).await?;
    l.backup("b2").await?;
    assert_ne!(anchor(&l)?["seq"], seq, "the newer backup moved the anchor on");

    let fresh = TestDb::empty().await?;
    let mut settings = fresh.settings(&l.dir.path().join("fresh"))?;
    settings.secrets_key = l.settings.secrets_key.clone(); // the key the backup was made under
    settings.anchor_dir = Some(l.settings.anchor_path()); // the same data volume
    settings.keyring_dir = Some(l.settings.keyring_path()); // the same keyring mount
    let report = ownhealth_ops::restore(&settings, &older).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert_eq!(report["audit_anchor"]["seq"], seq, "{report:#}");
    assert_eq!(report["audit_anchor_error"], Value::Null, "{report:#}");
    let a = anchor(&l)?;
    assert_eq!(
        (a["seq"].clone(), a["hash"].clone(), a["at"].clone()),
        (
            Value::from(seq),
            Value::from(hash),
            manifest(&older)?["created_at"].clone()
        )
    );
    let mut c = fresh.pool.acquire().await?;
    let s = ownhealth_ops::audit_status(&settings, &mut c).await?;
    assert_eq!(
        (s["ok"].clone(), s["anchor"]["state"].clone()),
        (Value::Bool(true), Value::from("ok")),
        "{s:#}"
    );
    Ok(())
}

/// A restore checks the anchor folder as a backup does (review PR 75, N2): it never anchors through
/// a symlinked folder or into one others may write. The restore itself still succeeds, and its
/// report says why no anchor was set.
#[cfg(unix)]
#[tokio::test]
async fn a_restore_does_not_anchor_in_a_folder_others_may_write() -> R {
    use std::os::unix::fs::PermissionsExt;
    let l = live().await?;
    let older = l.backup("b1").await?;
    let shared = l.dir.path().join("shared");
    fs::create_dir_all(&shared)?;
    let link = l.dir.path().join("anchor-link");
    std::os::unix::fs::symlink(&shared, &link)?;
    for (dir, m, why) in [
        (link.clone(), 0o700, "the anchor folder is a symlink"),
        (shared.clone(), 0o775, "the anchor folder is group-writable"),
        (shared.clone(), 0o777, "the anchor folder is world-writable"),
    ] {
        fs::set_permissions(&shared, fs::Permissions::from_mode(m))?;
        let fresh = TestDb::empty().await?;
        let mut settings = fresh.settings(&l.dir.path().join(format!("fresh-{m:o}")))?;
        settings.secrets_key = l.settings.secrets_key.clone(); // the key the backup was made under
        settings.anchor_dir = Some(dir);
        settings.keyring_dir = Some(l.settings.keyring_path());
        let report = ownhealth_ops::restore(&settings, &older).await?;
        assert_eq!(report["ok"], true, "{why}: {report:#}");
        assert_eq!(report["audit_anchor"], Value::Null, "{why}: {report:#}");
        let said = report["audit_anchor_error"].as_str().unwrap_or_default();
        assert!(said.contains(why), "{why}: {report:#}");
        assert!(
            !shared.join("audit-head.json").exists(),
            "{why}: the restore wrote an anchor"
        );
        assert_eq!(mode(&shared)?, m, "{why}: the folder mode changed");
    }
    Ok(())
}

/// The page's check (review PR 75, M1): loads at once run one full check, and a young verdict is
/// reused while the newest entry and the anchor stay the same. A new entry checks again.
#[tokio::test]
async fn page_loads_at_once_run_one_full_check() -> R {
    let l = live().await?;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..10 {
        let (settings, pool) = (l.settings.clone(), l.db.pool.clone());
        set.spawn(async move { ownhealth_ops::audit_status_shared(&settings, &pool).await });
    }
    while let Some(r) = set.join_next().await {
        assert_eq!(r??["ok"], true);
    }
    assert_eq!(ownhealth_ops::audit_status_checks(&l.settings).await, 1);
    add_events(&l, 1).await?;
    let s = ownhealth_ops::audit_status_shared(&l.settings, &l.db.pool).await?;
    assert_eq!(s["verified"], l.head().await?.0);
    assert_eq!(ownhealth_ops::audit_status_checks(&l.settings).await, 2);
    Ok(())
}

#[tokio::test]
async fn verify_restore_checks_the_head_in_the_manifest() -> R {
    let l = live().await?;
    let dest = l.backup("b1").await?;
    let ok = ownhealth_ops::verify_restore(&l.settings, &dest).await?;
    assert_eq!(ok["ok"], true, "{ok:#}");
    assert_eq!(ok["audit_chain"]["ok"], true);

    let original = manifest(&dest)?;
    for (key, value) in [
        ("hash", Value::from("0".repeat(64))),
        (
            "seq",
            Value::from(original["audit_chain"]["seq"].as_i64().unwrap_or(0) + 1),
        ),
        // an entry edited with no rehash keeps the head, and only `verified` tells
        (
            "verified",
            Value::from(original["audit_chain"]["verified"].as_i64().unwrap_or(0) - 1),
        ),
    ] {
        let mut m = original.clone();
        m["audit_chain"][key] = value;
        fs::write(dest.join("manifest.json"), serde_json::to_vec(&m)?)?;
        let bad = ownhealth_ops::verify_restore(&l.settings, &dest).await?;
        assert_eq!(bad["ok"], false, "{key}: {bad:#}");
        assert_eq!(bad["audit_chain"]["ok"], false, "{key}");
    }
    Ok(())
}

/// A backup from before the chain has no head in its manifest: verify-restore has nothing to compare
/// and says so, instead of failing every older backup.
#[tokio::test]
async fn an_older_manifest_without_a_head_is_not_a_failure() -> R {
    let l = live().await?;
    let dest = l.backup("b1").await?;
    let mut m = manifest(&dest)?;
    if let Some(o) = m.as_object_mut() {
        o.shift_remove("audit_chain");
    }
    fs::write(dest.join("manifest.json"), serde_json::to_vec(&m)?)?;
    let r = ownhealth_ops::verify_restore(&l.settings, &dest).await?;
    assert_eq!(r["ok"], true, "{r:#}");
    assert_eq!(r["audit_chain"], Value::Null);
    Ok(())
}
