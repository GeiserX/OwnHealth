//! Process-wide handles (`runtime.py`): settings, the pool and the blob store. The binary builds
//! one `Runtime` and hands clones to the HTTP state, the MCP service and the job workers.

use std::sync::Arc;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::blobs::BlobStore;
use crate::config::Settings;
use crate::jobs::JobContext;

#[derive(Debug, Clone)]
pub struct Runtime {
    pub settings: Arc<Settings>,
    pub pool: PgPool,
    pub blobs: BlobStore,
}

impl Runtime {
    /// Connect lazily (the first query opens the pool), so `serve` starts and `/readyz` reports a
    /// database that is not up yet instead of the process exiting.
    ///
    /// Every connection starts in read committed, whatever the database default is: the audit chain
    /// refuses rows from any other level (migrations/1250_audit_chain.sql). A read that needs one
    /// snapshot still sets repeatable read on its own transaction.
    pub fn new(settings: Settings) -> anyhow::Result<Runtime> {
        let options = settings
            .sqlx_url()
            .parse::<PgConnectOptions>()?
            .options([("default_transaction_isolation", "read committed")]);
        let pool = PgPoolOptions::new().max_connections(20).connect_lazy_with(options);
        Runtime::with_pool(settings, pool)
    }

    pub fn with_pool(settings: Settings, pool: PgPool) -> anyhow::Result<Runtime> {
        let blobs = BlobStore::new(settings.blobs_path())?;
        crate::access::set_env_majority(settings.age_of_majority);
        Ok(Runtime {
            settings: Arc::new(settings),
            pool,
            blobs,
        })
    }

    pub fn job_context(&self) -> JobContext {
        JobContext::new(self.pool.clone(), self.settings.clone(), self.blobs.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditEvent, write_audit};
    use crate::testing::TestDb;

    #[tokio::test]
    async fn the_pool_audits_reads_when_the_database_default_is_serializable() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "alter database \"{}\" set default_transaction_isolation = 'serializable'",
            db.name
        )))
        .execute(&db.pool)
        .await?;
        let dir = tempfile::tempdir()?;
        let rt = Runtime::new(db.settings(dir.path())?)?;
        let mut tx = rt.pool.begin().await?;
        write_audit(&mut tx, &AuditEvent::new("system", "job", "read")).await?;
        tx.commit().await?;
        // a read that asks for its own snapshot still gets it
        let mut tx = rt.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read")
            .execute(&mut *tx)
            .await?;
        let level: String = sqlx::query_scalar("show transaction_isolation").fetch_one(&mut *tx).await?;
        tx.rollback().await?;
        assert_eq!(level, "repeatable read");
        rt.pool.close().await;
        Ok(())
    }
}
