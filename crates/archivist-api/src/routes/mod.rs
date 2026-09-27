//! HTTP route handlers, grouped by API area.

mod auth;
mod metrics;
mod prompts;
mod providers;
mod settings;
mod users;

pub(crate) use auth::*;
pub(crate) use metrics::*;
pub(crate) use prompts::*;
pub(crate) use providers::*;
pub(crate) use settings::*;
pub(crate) use users::*;
