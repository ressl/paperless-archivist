use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use aes_gcm::aead::{Aead, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{Context, Result, anyhow};
use archivist_core::{
    AiArtifactStorageMode, AuditEventInput, BacklogCounts, DashboardBacklogPoint,
    DashboardComparison, DashboardCostBucket, DashboardLiveFailure, DashboardLiveJob,
    DashboardLiveLlmEvent, DashboardLiveRun, DashboardLiveStatus, DashboardRange,
    DashboardStageStatus, DashboardStats, DashboardStatusCount, DashboardTimeBucket,
    DocumentChatSource, DocumentInventoryItem, DuplicateDocument, DuplicateGroup,
    LanguageDetection, NeedsAttentionItem, ProcessingMode, ProviderUsageStats, QualityStats, Role,
    RuntimeSettings, ServiceProcessingStatus, Stage, WorkflowRules, WorkflowSafetyStatus,
    redact_sensitive_json,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, Timelike, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnection, PgPoolOptions, PgRow};
use sqlx::{Connection, PgPool, Postgres, QueryBuilder, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

mod transitions;

mod artifacts;
mod audit;
mod chat;
mod inventory;
mod jobs;
mod pool;
mod reviews;
mod runs;
mod settings;
mod startup_repairs;
mod stats;
mod users;

pub use artifacts::*;
pub use audit::*;
pub use chat::*;
pub use inventory::*;
pub use jobs::*;
pub use pool::*;
pub use reviews::*;
pub use runs::*;
pub use settings::*;
pub use startup_repairs::*;
pub use stats::*;
pub use users::*;

pub use transitions::{
    JobStatus, JobTransition, RECOVERED_STUCK_RUN_ERROR, ReviewStatus, ReviewTransition, RunStatus,
    RunTransition,
};
use transitions::{
    mirror_run_status_tx, revert_review_from_applying_tx, review_revert_target,
    sql_active_job_statuses, sql_active_run_statuses, sql_terminal_run_statuses,
    sql_terminal_stage_statuses, transition_run_tx, transition_runs_tx,
};

pub type DbPool = PgPool;
/// Transaction handle for callers that batch several helpers in one TX
/// without depending on sqlx directly (worker sync batches, #408).
pub type DbTransaction<'a> = Transaction<'a, Postgres>;

/// A referenced aggregate does not exist (or, for API tokens, is already
/// revoked). An expected client condition, mapped to 404 by the API. #441
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum NotFoundError {
    #[error("user does not exist")]
    User,
    #[error("API token not found or already revoked")]
    ApiToken,
    #[error("prompt does not exist")]
    Prompt,
}

#[cfg(test)]
mod tests;
