//! Shared bundle every D-Bus interface implementation needs:
//! state, profile store, authorization checker, backend-ops router,
//! and the live zbus connection (for object registration).
//!
//! Holding everything in one Arc lets us add new dependencies
//! without rippling constructor signatures across every interface.

use std::sync::Arc;

use nexus_profile_store::ProfileStore;
use tokio::sync::RwLock;

use crate::authz::AuthChecker;
use crate::backend_ops::BackendOps;
use crate::properties::PropertyBatcher;
use crate::rate_limit::RateLimiter;
use crate::state::State;

/// All the shared capabilities a `#[zbus::interface]` impl needs to
/// service a mutating method.
pub struct Services {
    pub state: Arc<RwLock<State>>,
    pub profile_store: Arc<dyn ProfileStore>,
    pub auth: Arc<dyn AuthChecker>,
    pub ops: Arc<dyn BackendOps>,
    /// Hot-property coalescing buffer (DD-006 §12.2).
    pub batcher: Arc<PropertyBatcher>,
    /// Per-sender rate limiter (DD-006 §15).
    pub rate_limiter: Arc<RateLimiter>,
}

impl Services {
    pub fn new(
        state: Arc<RwLock<State>>,
        profile_store: Arc<dyn ProfileStore>,
        auth: Arc<dyn AuthChecker>,
        ops: Arc<dyn BackendOps>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            state,
            profile_store,
            auth,
            ops,
            batcher: Arc::new(PropertyBatcher::new()),
            rate_limiter,
        }
    }
}
