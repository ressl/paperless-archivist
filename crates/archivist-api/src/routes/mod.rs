//! HTTP route handlers, grouped by API area.

mod auth;
mod chat;
mod dashboard;
mod metrics;
mod paperless;
mod prompts;
mod providers;
mod settings;
mod users;

pub(crate) use auth::*;
pub(crate) use chat::*;
pub(crate) use dashboard::*;
pub(crate) use metrics::*;
pub(crate) use paperless::*;
pub(crate) use prompts::*;
pub(crate) use providers::*;
pub(crate) use settings::*;
pub(crate) use users::*;
