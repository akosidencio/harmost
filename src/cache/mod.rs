//! Cache key construction and response shareability.
//!
//! Harmost implements Pingora's `Storage` trait rather than bringing its own
//! store, and uses `pingora-cache`'s cache lock for request coalescing. See
//! `spike/pingora-cache/FINDINGS.md` for how that was decided.

pub mod key;
pub mod policy;
pub mod store;

pub use key::{CacheKey, KeyBuilder};
pub use policy::{BypassReason, Disposition, Shareability};
pub use store::BoundedStore;

/// Private marker carried only in cache metadata while an in-flight response
/// is being followed. It is stripped before every downstream response.
pub(crate) const TRANSIENT_HEADER: &str = "x-harmost-transient-internal";
