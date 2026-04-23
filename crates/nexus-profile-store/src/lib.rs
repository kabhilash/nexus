//! Profile Store — on-disk persistence for Ethernet, Wi-Fi, GNSS,
//! and Bluetooth profiles. See DD-007.
//!
//! Phase 2: plaintext credentials, atomic writes, `0600` file mode.
//! Phase 3 (next prompt) replaces the plaintext credential form
//! with `EncryptedBlob`. The in-memory types that carry
//! [`crate::secret::SecretString`] stay the same across both phases.

pub mod error;
pub mod fs_store;
pub mod secret;
pub mod trait_def;
pub mod types;

pub use error::{Result, StoreError};
pub use fs_store::{ProfileFileStore, ssid_hash};
pub use secret::SecretString;
pub use trait_def::{ProfileKind, ProfileRef, ProfileStore, RotateReport};
pub use types::{
    Dot1xEapConfig, Dot1xEapConfigOnDisk, EapMethod, ProfileMetadata,
    bluetooth::BluetoothProfile,
    ethernet::{
        Dot1xSettings, Dot1xSettingsOnDisk, EthInterfaceSettings, EthernetProfile,
        EthernetProfileOnDisk,
    },
    gnss::GnssDeviceProfile,
    wifi::{
        SecurityConfig, SecurityConfigOnDisk, WifiNetworkSettings, WifiNetworkSettingsOnDisk,
        WifiProfile, WifiProfileOnDisk, WpaPsk, WpaPskOnDisk,
    },
};
