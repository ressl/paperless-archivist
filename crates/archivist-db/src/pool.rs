//! Connection pool and schema migrations.

use super::*;

pub async fn connect(database_url: &str, max_connections: u32) -> Result<DbPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await
        .context("connect to PostgreSQL")
}

pub async fn migrate(pool: &DbPool) -> Result<()> {
    let migrations_dir =
        std::env::var("ARCHIVIST_MIGRATIONS_DIR").unwrap_or_else(|_| "migrations".to_owned());
    sqlx::migrate::Migrator::new(Path::new(&migrations_dir))
        .await
        .with_context(|| format!("load database migrations from {migrations_dir}"))?
        .run(pool)
        .await
        .context("run database migrations")
}
