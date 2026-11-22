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
    assert!(!l.dest("b2").join("manifest.json").exists() && !l.dest("b2").join("db.dump").exists());

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
