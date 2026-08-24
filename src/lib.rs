//! Harmost — an origin workload governor for server-rendered applications.
//!
//! The name is Greek: a *harmost* (ἁρμοστής, from ἁρμόζω, "to fit, to keep in
//! proper adjustment") was an official posted to hold a system in correct
//! adjustment. That is this crate's job for an SSR origin.
//!
//! Three primitives carry the product, and all three are pure logic that can be
//! tested without a proxy runtime attached:
//!
//! * [`cache::key`] — cache key construction. Security-critical: a key that is
//!   too coarse serves one user's page to another.
//! * [`cache::policy`] — whether a response may be shared at all, evaluated
//!   both before and *after* the origin responds.
//! * [`admission`] — bounding how much origin work may be in flight.
//!
//! The Pingora proxy layer sits on top of these pure policy components.

pub mod admission;
pub mod cache;
pub mod classifier;
pub mod config;
pub mod policy;
pub mod proxy;
pub mod telemetry;
pub mod upstream;

pub use classifier::RequestClass;
pub use config::Config;
