//! Cross-process ownership of a logical Paperless apply source (#452).

use super::*;

/// A session advisory lock spans committed intent writes and the HTTP call.
/// Reuse its connection for those writes so a full pool of active applies
/// cannot deadlock waiting for a second connection.
pub struct ApplySourceLock {
    connection: Option<sqlx::PoolConnection<Postgres>>,
    source_key: String,
}

impl ApplySourceLock {
    pub fn connection(&mut self) -> &mut PgConnection {
        self.connection.as_mut().expect("apply lock connection")
    }

    /// Return a clean session to the pool only after PostgreSQL confirms the
    /// unlock. Cancellation or any error closes the session instead.
    pub async fn release(mut self) -> Result<()> {
        let source_key = self.source_key.clone();
        let unlocked: bool = sqlx::query_scalar(
            "select pg_advisory_unlock(hashtextextended('paperless-apply:' || $1, 0))",
        )
        .bind(source_key)
        .fetch_one(self.connection())
        .await?;
        if !unlocked {
            return Err(anyhow!("Paperless apply source lock was lost"));
        }
        self.connection.take();
        Ok(())
    }
}

impl Drop for ApplySourceLock {
    fn drop(&mut self) {
        if let Some(connection) = &mut self.connection {
            // A cancelled acquisition may already have obtained the lock at
            // the server. Never return a possibly locked session to the pool.
            connection.close_on_drop();
        }
    }
}

/// Do not wait behind an active API apply, drain or recovery worker. A crash
/// releases the lock with its PostgreSQL session; no timeout can expire while
/// the original HTTP request is still executing.
pub async fn try_lock_apply_source(
    pool: &DbPool,
    source_key: &str,
) -> Result<Option<ApplySourceLock>> {
    let mut guard = ApplySourceLock {
        connection: Some(pool.acquire().await?),
        source_key: source_key.to_owned(),
    };
    let acquired: bool = sqlx::query_scalar(
        "select pg_try_advisory_lock(hashtextextended('paperless-apply:' || $1, 0))",
    )
    .bind(source_key)
    .fetch_one(guard.connection())
    .await?;
    if !acquired {
        // The query completed and did not acquire a lock, so this session is
        // safe to reuse without issuing an unlock for somebody else's lock.
        guard.connection.take();
        return Ok(None);
    }
    Ok(Some(guard))
}
