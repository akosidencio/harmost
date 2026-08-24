//! The `harmost.yaml` shape.
//!
//! Every struct is `deny_unknown_fields`. A typo'd key is a silent policy
//! change otherwise, and a silent policy change in *this* config means either
//! an unprotected origin or a cache serving something it should not.

use super::units::{Bytes, Dur};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub server: Server,
    pub origin: Origin,
    #[serde(default)]
    pub health: Option<Health>,
    #[serde(default)]
    pub cache: CacheDefaults,
    #[serde(default)]
    pub coalesce: CoalesceDefaults,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub overload: Overload,
    #[serde(default)]
    pub deployment: Deployment,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub telemetry: Telemetry,
    /// Emit the `X-Harmost` cache-status header. Off by default: it tells an
    /// attacker whether their probe was cached, which is reconnaissance for
    /// cache poisoning.
    #[serde(default)]
    pub debug_headers: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default = "default_listen")]
    pub listen: String,
}

impl Default for Server {
    fn default() -> Self {
        Server { listen: default_listen() }
    }
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub upstreams: Vec<String>,
    #[serde(default)]
    pub load_balancing: LoadBalancing,
    #[serde(default)]
    pub concurrency: Concurrency,
}

/// v0.1 ships round-robin and hash-by-path only.
///
/// `least_loaded` is deliberately absent: it belongs with the latency
/// observations and circuit breaking that arrive together later, and
/// round-robin is indistinguishable from it once admission control is already
/// bounding in-flight work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancing {
    #[default]
    RoundRobin,
    /// Route a given path to a consistent backend. Costs nothing and improves
    /// the *origin's* own warmth — its in-process render cache, module graph
    /// and JIT state are all path-correlated.
    HashByPath,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Concurrency {
    pub max: usize,
    #[serde(default)]
    pub queue: Queue,
}

impl Default for Concurrency {
    fn default() -> Self {
        Concurrency { max: 500, queue: Queue::default() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Queue {
    /// Hard bound. An unbounded queue converts an origin overload into a
    /// proxy overload and defers the failure instead of preventing it.
    pub max: usize,
    pub timeout: Dur,
}

impl Default for Queue {
    fn default() -> Self {
        Queue { max: 0, timeout: Dur::ZERO }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub path: String,
    pub interval: Dur,
    pub timeout: Dur,
    #[serde(default = "one")]
    pub healthy_after: u32,
    #[serde(default = "three")]
    pub unhealthy_after: u32,
}

fn one() -> u32 { 1 }
fn three() -> u32 { 3 }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheDefaults {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub store: Store,
    #[serde(default = "default_max_memory")]
    pub max_memory: Bytes,
    #[serde(default = "default_max_body")]
    pub max_body_size: Bytes,
    /// Origin `Cache-Control` wins unless a route explicitly opts out. Global
    /// opt-out is not offered at any level; see `Route.cache.override_origin`.
    #[serde(default = "yes")]
    pub respect_origin: bool,
}

impl Default for CacheDefaults {
    fn default() -> Self {
        CacheDefaults {
            enabled: true,
            store: Store::Memory,
            max_memory: default_max_memory(),
            max_body_size: default_max_body(),
            respect_origin: true,
        }
    }
}

fn yes() -> bool { true }
fn default_max_memory() -> Bytes { Bytes(512 * 1024 * 1024) }
fn default_max_body() -> Bytes { Bytes(4 * 1024 * 1024) }

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Store {
    #[default]
    Memory,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoalesceDefaults {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// How long a waiter will wait on the in-flight leader.
    ///
    /// Defaults to `None`, meaning "as long as the origin request itself may
    /// take" — resolved from `timeouts.origin` at load. A shorter value than
    /// the origin timeout releases every waiter *before* the work they are
    /// waiting on can possibly finish, which manufactures the stampede this
    /// exists to prevent.
    #[serde(default)]
    pub wait_timeout: Option<Dur>,
    /// What a waiter does when its wait expires. Independent execution is off
    /// by default for the reason above.
    #[serde(default)]
    pub on_timeout: OnCoalesceTimeout,
}

impl Default for CoalesceDefaults {
    fn default() -> Self {
        CoalesceDefaults { enabled: true, wait_timeout: None, on_timeout: OnCoalesceTimeout::default() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnCoalesceTimeout {
    /// Serve stale if a permitted entry exists, otherwise shed.
    #[default]
    StaleOrShed,
    /// Re-enter admission individually. Rate-limited by the admission
    /// controller, never released as a group.
    Requeue,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    #[serde(default = "d_500ms")]
    pub connect: Dur,
    #[serde(default = "d_5s")]
    pub first_byte: Dur,
    #[serde(default = "d_30s")]
    pub idle: Dur,
    /// Ceiling on one origin request end to end. Also the floor for
    /// `coalesce.wait_timeout`.
    #[serde(default = "d_30s")]
    pub origin: Dur,
    /// How long Harmost will wait on a stalled *client* before abandoning the
    /// downstream write. Without this a slow reader occupies a slot forever.
    #[serde(default = "d_30s")]
    pub downstream_write: Dur,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect: d_500ms(),
            first_byte: d_5s(),
            idle: d_30s(),
            origin: d_30s(),
            downstream_write: d_30s(),
        }
    }
}

fn d_500ms() -> Dur { Dur(std::time::Duration::from_millis(500)) }
fn d_5s() -> Dur { Dur(std::time::Duration::from_secs(5)) }
fn d_30s() -> Dur { Dur(std::time::Duration::from_secs(30)) }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overload {
    #[serde(default = "status_503")]
    pub status: u16,
    #[serde(default = "d_1s")]
    pub retry_after: Dur,
}

impl Default for Overload {
    fn default() -> Self {
        Overload { status: status_503(), retry_after: d_1s() }
    }
}

fn status_503() -> u16 { 503 }
fn d_1s() -> Dur { Dur(std::time::Duration::from_secs(1)) }

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    /// Static build identifier, mixed into every cache key.
    #[serde(default)]
    pub id: Option<String>,
    /// Or read it from a response header the origin sets.
    #[serde(default)]
    pub id_header: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub id: String,
    #[serde(rename = "match")]
    pub matcher: Matcher,
    #[serde(default)]
    pub class: Option<ClassOverride>,
    #[serde(default)]
    pub cache: Option<RouteCache>,
    #[serde(default)]
    pub coalesce: Option<RouteCoalesce>,
    #[serde(default)]
    pub concurrency: Option<Concurrency>,
}

/// `match: "/products/**"` and the expanded table form both parse.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Matcher {
    Path(String),
    Detailed(DetailedMatcher),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailedMatcher {
    #[serde(default)]
    pub host: Option<String>,
    pub path: String,
    #[serde(default)]
    pub methods: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassOverride {
    Static,
    PublicSsr,
    PublicDynamic,
    PrivateDynamic,
    Streaming,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCache {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub mode: Option<CacheMode>,
    #[serde(default)]
    pub ttl: Option<Ttl>,
    #[serde(default)]
    pub stale_while_revalidate: Option<Dur>,
    #[serde(default)]
    pub stale_if_error: Option<Dur>,
    #[serde(default)]
    pub query: Option<QueryPolicy>,
    #[serde(default)]
    pub vary: Option<VaryPolicy>,
    /// Store this route's responses even when the origin says not to.
    ///
    /// This exists because a dynamically-rendered Next.js route answers with
    /// `Cache-Control: private, no-cache, no-store, max-age=0, must-revalidate`
    /// — so without an override, the microcache never engages on precisely the
    /// routes worth protecting. It is per-route, never global, and validation
    /// refuses to combine it with a private class or a cookie-bearing route.
    #[serde(default)]
    pub override_origin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    /// Follow origin headers exactly, with no route ceiling.
    Origin,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ttl {
    /// Ceiling applied to whatever the origin asked for. Shrinks, never grows.
    #[serde(default)]
    pub max: Option<Dur>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryPolicy {
    pub mode: QueryMode,
    #[serde(default)]
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryMode {
    /// Only the listed keys enter the cache key. Unlisted keys are dropped,
    /// which is what stops `?cachebust=<random>` from minting a fresh key —
    /// and therefore a fresh render — on every request.
    Include,
    /// Everything except the listed keys enters the key.
    Exclude,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaryPolicy {
    #[serde(default)]
    pub headers: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCoalesce {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Collapse concurrent equivalent requests even when the origin says
    /// `no-store`. Weaker than `cache.override_origin`: nothing is persisted,
    /// the window is one render, and every waiter would otherwise have
    /// rendered against the same origin state anyway.
    #[serde(default)]
    pub override_origin: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    #[serde(default)]
    pub prometheus: Option<Prometheus>,
    #[serde(default)]
    pub logging: Logging,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prometheus {
    pub listen: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logging {
    #[serde(default)]
    pub format: LogFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Json,
    Text,
}
