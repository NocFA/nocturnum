//! The REST surface, a thin adapter over the operation layer in `nocturnum-query`, described
//! in `docs/api/v1.md`.
//!
//! [`Api`] holds what every request shares, and [`Api::router`] builds the axum router.
//! Collections load after the listener is up, so health checks answer during a long
//! recovery, and every `/v1` request answers `503 unavailable` until [`Api::set_registry`]
//! has been called.
//!
//! Handlers run their work on tokio's blocking threads, since storage calls block on locks
//! and fsyncs.

mod auth;
pub mod cron;
pub mod dto;
pub mod error;
mod extract;
mod metrics;
pub mod registry;
mod routes;
pub mod schedule;

use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::extract::DefaultBodyLimit;

pub use error::ApiError;
pub use registry::{Registry, RegistryOptions};
pub use schedule::{Schedule, ScheduleState, Scheduler};

/// The default request body limit, 32 MiB.
pub const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ApiOptions {
    /// Allows everything.
    pub admin_key: Option<String>,
    /// Allows `GET` requests and the point reads sent with `POST`.
    pub read_only_key: Option<String>,
    pub max_body_bytes: usize,
    /// Host names accepted while no keys are set, beyond IP addresses and `localhost`.
    pub allowed_hosts: Vec<String>,
}

impl Default for ApiOptions {
    fn default() -> Self {
        ApiOptions {
            admin_key: None,
            read_only_key: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            allowed_hosts: Vec::new(),
        }
    }
}

pub struct Api {
    keys: auth::Keys,
    allowed_hosts: Vec<String>,
    max_body_bytes: usize,
    /// Declared before `registry`, so it stops, letting go of its handle on the registry,
    /// before the registry is dropped and closes the collections.
    scheduler: OnceLock<Scheduler>,
    registry: OnceLock<Arc<Registry>>,
    metrics: metrics::Metrics,
}

impl Api {
    pub fn new(options: ApiOptions) -> Arc<Api> {
        Arc::new(Api {
            keys: auth::Keys::new(
                options.admin_key.as_deref(),
                options.read_only_key.as_deref(),
            ),
            allowed_hosts: options.allowed_hosts,
            max_body_bytes: options.max_body_bytes,
            scheduler: OnceLock::new(),
            registry: OnceLock::new(),
            metrics: metrics::Metrics::default(),
        })
    }

    /// Starts serving collections, once they are loaded, and running their scheduled
    /// snapshots. Later calls are ignored.
    pub fn set_registry(&self, registry: Registry) {
        let registry = Arc::new(registry);
        if self.registry.set(registry.clone()).is_ok() {
            let _ = self.scheduler.set(Scheduler::start(registry));
        }
    }

    /// The collections, or `503 unavailable` while they are still loading.
    pub fn registry(&self) -> Result<Arc<Registry>, ApiError> {
        self.registry
            .get()
            .cloned()
            .ok_or_else(|| ApiError::unavailable("collections are still loading"))
    }

    pub fn router(self: &Arc<Self>) -> Router {
        routes::routes()
            .route_layer(axum::middleware::from_fn_with_state(
                self.clone(),
                auth::guard,
            ))
            .route_layer(axum::middleware::from_fn(metrics::tag))
            .merge(routes::health())
            .fallback(routes::not_found)
            .method_not_allowed_fallback(routes::method_not_allowed)
            .layer(DefaultBodyLimit::max(self.max_body_bytes))
            .layer(axum::middleware::from_fn_with_state(
                self.clone(),
                metrics::track,
            ))
            .with_state(self.clone())
    }
}
