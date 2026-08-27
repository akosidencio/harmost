//! Connection-level facts that the rest of Harmost treats as inputs: who the
//! client is, and which scheme they used.
//!
//! Both arrive as *claims* when Harmost runs behind a load balancer or a CDN,
//! and both feed decisions that matter — the scheme is part of the cache key,
//! and the client address is what an operator reads during an incident. So the
//! parsing lives here, apart from the proxy runtime, where it can be tested
//! against the shapes an attacker actually sends.

pub mod forwarded;
