//! Profile Store — on-disk persistence for Ethernet, Wi-Fi, GNSS,
//! and Bluetooth profiles with per-field ChaCha20-Poly1305
//! encryption, schema migration, master-key rotation, and
//! quarantine for corrupt files. See DD-007.

pub mod crypto;
pub mod error;
pub mod fs_store;
pub mod keys;
pub mod metrics;
pub mod migrate;
pub mod quarantine;
pub mod rotate;
pub mod secret;
pub mod trait_def;
pub mod types;

pub use crypto::{
    ChaChaCipher, Cipher, CipherError, ENC_TAG_V1, EncryptedBlob, associated_data, derive_file_key,
};
pub use error::{Result, StoreError};
pub use fs_store::{ProfileFileStore, ssid_hash};
pub use keys::{DerivedKeySource, FileKeySource, InMemoryKeySource, KeyError, MasterKeySource};
pub use migrate::{CURRENT_STORE_VERSION, MigrationReport};
pub use rotate::{generate_master_key, rotate_to_key};
pub use secret::SecretString;
pub use trait_def::{ProfileKind, ProfileRef, ProfileStore, RotateReport};
pub use types::{
    Dot1xEapConfig, Dot1xEapConfigOnDisk, EapMethod, ProfileMetadata,
    bluetooth::BluetoothProfile,
    ethernet::{
        Dot1xSettings, Dot1xSettingsOnDisk, EthInterfaceSettings, EthernetProfile,
        EthernetProfileOnDisk, decrypt_ethernet, encrypt_ethernet,
    },
    gnss::GnssDeviceProfile,
    wifi::{
        SecurityConfig, SecurityConfigOnDisk, WifiNetworkSettings, WifiNetworkSettingsOnDisk,
        WifiProfile, WifiProfileOnDisk, WpaPsk, WpaPskOnDisk, decrypt_wifi, encrypt_wifi,
    },
};

#[cfg(feature = "tpm")]
pub use keys::TpmKeySource;
