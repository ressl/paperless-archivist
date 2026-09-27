//! HTTP route handlers, grouped by API area.

mod auth;
mod metrics;
mod users;

pub(crate) use auth::*;
pub(crate) use metrics::*;
pub(crate) use users::*;
