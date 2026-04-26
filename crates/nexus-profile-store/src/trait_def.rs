//! The [`ProfileStore`] trait + its small supporting types.

use std::time::Duration;

use async_trait::async_trait;
use nexus_core::MacAddr;
use ulid::Ulid;

use crate::error::Result;
use crate::types::bluetooth::BluetoothProfile;
use crate::types::ethernet::EthernetProfile;
use crate::types::gnss::GnssDeviceProfile;
use crate::types::wifi::WifiProfile;

// `ProfileKind` is already canonical in `nexus_core` — re-export to
// avoid two definitions of the same enum.
pub use nexus_core::ProfileKind;

/// Cross-kind key into the store. Which variant the caller passes
/// determines how the store interprets the key.
#[derive(Debug, Clone, Copy)]
pub enum ProfileRef<'a> {
    /// Ethernet profile identified by interface name.
    Ethernet { ifname: &'a str },
    /// Wi-Fi profile identified by the SSID hash (not the raw SSID).
    Wifi { ssid_hash: &'a str },
    /// GNSS profile identified by ULID.
    Gnss { id: &'a Ulid },
    /// Bluetooth profile identified by ULID.
    Bluetooth { id: &'a Ulid },
}

impl<'a> ProfileRef<'a> {
    pub fn kind(&self) -> ProfileKind {
        match self {
            ProfileRef::Ethernet { .. } => ProfileKind::Ethernet,
            ProfileRef::Wifi { .. } => ProfileKind::Wifi,
            ProfileRef::Gnss { .. } => ProfileKind::Gnss,
            ProfileRef::Bluetooth { .. } => ProfileKind::Bluetooth,
        }
    }
}

/// Outcome of a successful [`ProfileStore::rotate_master_key`] call.
#[derive(Debug, Clone)]
pub struct RotateReport {
    pub profiles_rewritten: u32,
    pub duration: Duration,
}

/// The Profile Store trait. See DD-007 §5.1 for the full method
/// contract; doc comments here summarize — refer to the DD for
/// corner cases.
#[async_trait]
pub trait ProfileStore: Send + Sync {
    // ---- Ethernet ---------------------------------------------------

    /// Load every Ethernet profile. Malformed files are skipped
    /// with a `warn` log; the remainder still load. Order is
    /// ULID-ascending and stable, but carries no semantics.
    async fn load_ethernet(&self) -> Result<Vec<EthernetProfile>>;

    /// Load one Ethernet profile by interface name.
    async fn load_ethernet_profile(&self, ifname: &str) -> Result<Option<EthernetProfile>>;

    /// Persist an Ethernet profile. The profile's interface name is
    /// the on-disk filename.
    async fn put_ethernet(&self, profile: &EthernetProfile) -> Result<()>;

    /// Remove the Ethernet profile whose interface name is
    /// `ifname`. Not-found is `Ok(())` — the caller's invariant is
    /// "ensure absent", not "must have existed".
    async fn remove_ethernet(&self, ifname: &str) -> Result<()>;

    // ---- Wi-Fi ------------------------------------------------------

    async fn load_wifi(&self) -> Result<Vec<WifiProfile>>;

    async fn put_wifi(&self, profile: &WifiProfile) -> Result<()>;

    async fn remove_wifi(&self, ssid_hash: &str) -> Result<()>;

    // ---- GNSS -------------------------------------------------------

    async fn load_gnss(&self) -> Result<Vec<GnssDeviceProfile>>;

    async fn load_gnss_profile_by_path(
        &self,
        device_path: &str,
    ) -> Result<Option<GnssDeviceProfile>>;

    async fn put_gnss(&self, profile: &GnssDeviceProfile) -> Result<()>;

    async fn remove_gnss(&self, id: &Ulid) -> Result<()>;

    // ---- Bluetooth --------------------------------------------------

    async fn load_bluetooth(&self) -> Result<Vec<BluetoothProfile>>;

    async fn load_bluetooth_profile_by_address(
        &self,
        address: &MacAddr,
    ) -> Result<Option<BluetoothProfile>>;

    async fn put_bluetooth(&self, profile: &BluetoothProfile) -> Result<()>;

    async fn remove_bluetooth(&self, id: &Ulid) -> Result<()>;

    // ---- Cross-kind -------------------------------------------------

    /// Mark a profile as having invalid credentials. Idempotent.
    /// The filesystem implementation reads, mutates, and rewrites.
    async fn set_credentials_invalid(&self, reference: ProfileRef<'_>, invalid: bool)
    -> Result<()>;

    /// Stamp `WifiNetworkSettings::last_connected_at` on the
    /// referenced profile. The Wi-Fi backend calls this on every
    /// successful Connected transition so auto-select can use
    /// recency as a tiebreaker on the next boot. No-op for
    /// non-Wi-Fi `ProfileRef` variants. Idempotent.
    async fn set_last_connected(
        &self,
        reference: ProfileRef<'_>,
        when: chrono::DateTime<chrono::Utc>,
    ) -> Result<()>;

    /// Rotate the master key (§4.5). Phase 2 does not have an
    /// encryption layer, so this returns
    /// [`crate::error::StoreError::NotYetImplemented`].
    async fn rotate_master_key(&self) -> Result<RotateReport>;
}
