//! Shared types for the Nexus workspace.
//!
//! `nexus-core` holds every type that crosses crate boundaries —
//! event-bus payloads, the interface registry record, MAC and SSID
//! primitives, notification payloads, and profile metadata. It
//! contains no I/O, no async, and no networking logic; every other
//! Nexus crate depends on it for the types it produces or consumes.
//!
//! Top-level re-exports cover the common working set. See each
//! module for the full API.

pub mod address;
pub mod event;
pub mod interface;
pub mod metadata;
pub mod notification;
pub mod ssid;

pub use address::{BluetoothAddrExt, MacAddr, ParseMacAddrError};
pub use event::{
    AuthFailureReason, AuthState, BssCapabilities, BssInfo, BtAddressType, BtDeviceInfo,
    BtFailureReason, BtTransport, DisconnectReason, FixMode, GnssFix, NexusEvent, PairingAnswer,
    PairingJobId, PairingPromptData, PairingPromptKind, ProfileKind, SatInfo, SecurityMode,
    WifiState,
};
pub use interface::{InterfaceInfo, InterfaceKind, Nl80211IfType, OperState, PhyCapabilities};
pub use metadata::ProfileMetadata;
pub use notification::{NotificationData, NotificationValue};
pub use ssid::{InvalidSsid, SSID_MAX_LEN, Ssid};
