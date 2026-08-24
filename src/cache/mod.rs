//! Cache key construction and response shareability.
//!
//! The store itself is deliberately absent. Whether Harmost brings its own
//! `CacheStore` or implements Pingora's `Storage` trait on top of
//! `pingora-cache` is an open decision — `pingora-cache` already has a working
//! cache lock and `Vary` machinery, but its only `Storage` implementation is
//! documented "for testing only". The two modules here are needed either way.

pub mod key;
pub mod policy;

pub use key::{CacheKey, KeyBuilder};
pub use policy::{BypassReason, Disposition, Shareability};
