//! Per-interface lifecycle state for the Ethernet Backend. See
//! DD-002 §3.

use std::time::Instant;

use nexus_core::InterfaceInfo;
use nexus_profile_store::EthernetProfile;

/// State machine per DD-002 §3.1. The `AuthFailed` variant carries
/// its retry context inline so the main loop's select! can schedule
/// the next retry without a side table.
#[derive(Debug, Clone)]
pub enum EthInterfaceState {
    /// Interface just discovered, profile not yet loaded.
    Registered,
    /// Profile loaded, waiting for carrier.
    WaitingForCarrier,
    /// Carrier up, 802.1X not configured — LinkReady was emitted.
    LinkReady,
    /// Carrier up, 802.1X configured, authentication in progress.
    Authenticating,
    /// Carrier up and authenticated; LinkReady was emitted.
    Authenticated,
    /// Carrier up but authentication failed. Waiting to retry.
    AuthFailed { retry_after: Instant, attempts: u32 },
    /// Interface removed.
    Gone,
}

impl EthInterfaceState {
    /// Short label suitable for metric `state` tags.
    pub fn label(&self) -> &'static str {
        match self {
            EthInterfaceState::Registered => "registered",
            EthInterfaceState::WaitingForCarrier => "waiting_carrier",
            EthInterfaceState::LinkReady => "link_ready",
            EthInterfaceState::Authenticating => "authenticating",
            EthInterfaceState::Authenticated => "authenticated",
            EthInterfaceState::AuthFailed { .. } => "auth_failed",
            EthInterfaceState::Gone => "gone",
        }
    }

    /// True when the Ethernet Backend considers the link ready for
    /// layer-3 configuration.
    pub fn is_ready(&self) -> bool {
        matches!(
            self,
            EthInterfaceState::LinkReady | EthInterfaceState::Authenticated,
        )
    }
}

/// Per-interface record held by [`crate::backend::EthernetBackend`].
#[derive(Debug, Clone)]
pub struct EthInterfaceEntry {
    pub info: InterfaceInfo,
    pub profile: EthernetProfile,
    pub state: EthInterfaceState,
}

impl EthInterfaceEntry {
    pub fn new(info: InterfaceInfo, profile: EthernetProfile) -> Self {
        Self {
            info,
            profile,
            state: EthInterfaceState::WaitingForCarrier,
        }
    }

    /// True if the profile says 802.1X is enabled (and thus carrier-
    /// up → authenticating rather than carrier-up → link-ready).
    pub fn requires_auth(&self) -> bool {
        self.profile
            .dot1x
            .as_ref()
            .map(|d| d.enabled)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn state_labels_are_stable() {
        assert_eq!(EthInterfaceState::Registered.label(), "registered");
        assert_eq!(
            EthInterfaceState::WaitingForCarrier.label(),
            "waiting_carrier",
        );
        assert_eq!(EthInterfaceState::LinkReady.label(), "link_ready");
        assert_eq!(EthInterfaceState::Authenticating.label(), "authenticating");
        assert_eq!(EthInterfaceState::Authenticated.label(), "authenticated");
        assert_eq!(
            EthInterfaceState::AuthFailed {
                retry_after: Instant::now() + Duration::from_secs(1),
                attempts: 1,
            }
            .label(),
            "auth_failed",
        );
        assert_eq!(EthInterfaceState::Gone.label(), "gone");
    }

    #[test]
    fn is_ready_only_for_link_ready_and_authenticated() {
        assert!(EthInterfaceState::LinkReady.is_ready());
        assert!(EthInterfaceState::Authenticated.is_ready());
        assert!(!EthInterfaceState::Registered.is_ready());
        assert!(!EthInterfaceState::WaitingForCarrier.is_ready());
        assert!(!EthInterfaceState::Authenticating.is_ready());
        assert!(
            !EthInterfaceState::AuthFailed {
                retry_after: Instant::now(),
                attempts: 1
            }
            .is_ready(),
        );
        assert!(!EthInterfaceState::Gone.is_ready());
    }
}
