//! HTTP route handlers, grouped by API area.

mod audit;
mod auth;
mod chat;
mod dashboard;
mod inventory;
mod metadata_trace;
mod metrics;
mod operations;
mod paperless;
mod prompts;
mod providers;
mod reviews;
mod settings;
mod users;
mod webhooks;

pub(crate) use audit::*;
pub(crate) use auth::*;
pub(crate) use chat::*;
pub(crate) use dashboard::*;
pub(crate) use inventory::*;
pub(crate) use metadata_trace::*;
pub(crate) use metrics::*;
pub(crate) use operations::*;
pub(crate) use paperless::*;
pub(crate) use prompts::*;
pub(crate) use providers::*;
pub(crate) use reviews::*;
pub(crate) use settings::*;
pub(crate) use users::*;
pub(crate) use webhooks::*;
