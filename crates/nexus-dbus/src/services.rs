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
use crate::errors::DbusError;
use crate::properties::PropertyBatcher;
use crate::rate_limit::RateLimiter;
use crate::state::State;

/// Per-backend enable/disable flags plumbed in from `nexus.toml`.
/// A mutating method on a disabled backend returns
/// `fi.nexus.Error.FeatureDisabled` rather than doing any work.
/// Defaults to all-enabled so unit tests that build `Services`
/// directly don't have to re-state this.
#[derive(Debug, Clone, Copy)]
pub struct EnabledFeatures {
    pub ethernet: bool,
    pub wifi: bool,
    pub bluetooth: bool,
    pub gnss: bool,
}

impl Default for EnabledFeatures {
    fn default() -> Self {
        Self {
            ethernet: true,
            wifi: true,
            bluetooth: true,
            gnss: true,
        }
    }
}

/// Per-feature tag. The string form is the `feature` field returned
/// in `fi.nexus.Error.FeatureDisabled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Ethernet,
    Wifi,
    Bluetooth,
    Gnss,
}

impl Feature {
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::Ethernet => "ethernet",
            Feature::Wifi => "wifi",
            Feature::Bluetooth => "bluetooth",
            Feature::Gnss => "gnss",
        }
    }
}

impl EnabledFeatures {
    pub fn is_enabled(&self, feature: Feature) -> bool {
        match feature {
            Feature::Ethernet => self.ethernet,
            Feature::Wifi => self.wifi,
            Feature::Bluetooth => self.bluetooth,
            Feature::Gnss => self.gnss,
        }
    }

    /// Returns `Err(FeatureDisabled)` when the feature is off. Call
    /// from the top of every mutating method on a per-technology
    /// interface.
    pub fn require(&self, feature: Feature) -> crate::errors::Result<()> {
        if self.is_enabled(feature) {
            Ok(())
        } else {
            Err(DbusError::FeatureDisabled(feature.as_str().to_owned()))
        }
    }
}

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
    /// Which per-technology backends are enabled (DD-006 §11.1
    /// `FeatureDisabled`).
    pub enabled: EnabledFeatures,
}

impl Services {
    pub fn new(
        state: Arc<RwLock<State>>,
        profile_store: Arc<dyn ProfileStore>,
        auth: Arc<dyn AuthChecker>,
        ops: Arc<dyn BackendOps>,
        rate_limiter: Arc<RateLimiter>,
        enabled: EnabledFeatures,
    ) -> Self {
        Self {
            state,
            profile_store,
            auth,
            ops,
            batcher: Arc::new(PropertyBatcher::new()),
            rate_limiter,
            enabled,
        }
    }
}
