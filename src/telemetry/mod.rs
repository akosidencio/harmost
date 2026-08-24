//! Observability. Never a hard dependency for serving a request: if metrics
//! or logging fail, traffic continues.

pub mod logging;
pub mod metrics;
