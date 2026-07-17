//! PostgreSQL-only SQLx facade for Paperless Archivist.
//!
//! The upstream `sqlx` facade records all optional database drivers in
//! `Cargo.lock`, even when they are disabled. Re-exporting the small API subset
//! used by this project keeps the lockfile limited to PostgreSQL without
//! changing application call sites or enabling SQLx macros.

pub use sqlx_core::connection::Connection;
pub use sqlx_core::error::{Error, Result};
pub use sqlx_core::executor::Executor;
pub use sqlx_core::migrate;
pub use sqlx_core::query::{query, query_with};
pub use sqlx_core::query_builder::QueryBuilder;
pub use sqlx_core::query_scalar::{query_scalar, query_scalar_with};
pub use sqlx_core::raw_sql::{RawSql, raw_sql};
pub use sqlx_core::row::Row;
pub use sqlx_core::sql_str::AssertSqlSafe;
pub use sqlx_core::transaction::Transaction;
pub use sqlx_postgres::{
    self as postgres, PgConnection, PgExecutor, PgPool, PgTransaction, Postgres,
};
