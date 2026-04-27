//! The event bus type and every payload it carries.
//!
//! See `nexus-architecture.md` §6 for the canonical variant list.
//!
//! # Note on type placement
//!
//! The architecture places most of the payload types (`BtDeviceInfo`,
//! `WifiState`, `AuthState`, `GnssFix`, `SatInfo`, `BssInfo`,
//! `PairingJobId`, `PairingPromptKind`, `PairingPromptData`,
//! `BtFailureReason`, `ProfileKind`) in their respective backend
//! crates, with `nexus-core` re-exporting them via `pub use`.
//! Because those backend types reference `MacAddr` / `Ssid` /
//! `InterfaceInfo`, which live here, a re-export model would force a
//! `nexus-core → nexus-bluetooth → nexus-core` cycle. Until the
//! workspace is restructured to break that cycle, the canonical
//! definitions live in this module. Downstream crates should import
//! them through `nexus_core::*` so the eventual move is a one-line
//! change per call site.

use chrono::{DateTime, Utc};
use ulid::Ulid;

use crate::address::MacAddr;
use crate::interface::InterfaceInfo;
use crate::notification::NotificationData;
use crate::ssid::Ssid;

// ---------------------------------------------------------------------------
// NexusEvent
// ---------------------------------------------------------------------------

/// Single-type enum carried on the broadcast event bus. Every
/// Nexus-internal state change flows through this. See
/// `nexus-architecture.md` §6.
#[derive(Debug, Clone)]
pub enum NexusEvent {
    // --- Interface Monitor ---
    InterfaceDiscovered(InterfaceInfo),
    InterfaceRemoved {
        ifindex: u32,
    },
    CarrierChanged {
        ifindex: u32,
        up: bool,
    },
    OperstateChanged {
        ifindex: u32,
        state: crate::interface::OperState,
    },

    // --- Ethernet Backend ---
    EthAuthStateChanged {
        ifindex: u32,
        state: AuthState,
    },
    /// Lifecycle-state transition for an Ethernet interface. Emitted
    /// by the Ethernet Backend on every `EthInterfaceState` change so
    /// the D-Bus layer can mirror DD-006 §6.2 `fi.nexus.Ethernet`
    /// properties (`State`, `EapMethod`, `AuthFailureReason`).
    /// `state` uses the labels in DD-006 §6.2 (`waiting_carrier` /
    /// `link_ready` / `authenticating` / `authenticated` /
    /// `auth_failed`). `eap_method` is `Some` when the interface's
    /// profile has 802.1X enabled. `auth_failure_reason` is `Some`
    /// only when `state == "auth_failed"`.
    EthLifecycleStateChanged {
        ifindex: u32,
        state: String,
        eap_method: Option<String>,
        auth_failure_reason: Option<String>,
    },
    /// Carrier up AND authenticated (if required).
    EthLinkReady {
        ifindex: u32,
    },
    /// Carrier dropped or authentication ended.
    EthLinkLost {
        ifindex: u32,
    },
    /// `NameOwnerChanged` transition on the wired-auth backend's
    /// D-Bus name (e.g., `fi.w1.wpa_supplicant1`). Emitted by the
    /// auth-backend implementation. The Ethernet Backend uses this
    /// to fail any in-flight auth (DD-002 §9.2) and to refresh the
    /// `nexus_eth_auth_backend_available` gauge (§9.5).
    EthAuthBackendOwnerChanged {
        backend: String,
        present: bool,
    },

    // --- Wi-Fi Backend ---
    /// Emitted whenever the Wi-Fi Backend finishes a scan attempt
    /// (DD-003 §5.3). `success` mirrors wpa_supplicant's
    /// `ScanDone(success)` argument — `false` means the supplicant
    /// reported the scan as aborted (driver-initiated cancel,
    /// rfkill-mid-scan, etc.). `results` is always the latest
    /// supplicant cache snapshot, regardless of `success`, so
    /// downstream consumers (`fi.nexus.ScanResult` registration)
    /// stay in sync with whatever the cache currently contains.
    WifiScanComplete {
        ifindex: u32,
        success: bool,
        results: Vec<BssInfo>,
    },
    WifiStateChanged {
        ifindex: u32,
        state: WifiState,
    },
    WifiSignalPoll {
        ifindex: u32,
        rssi: i32,
        frequency: u32,
    },
    /// Associated, authenticated, keyed.
    WifiLinkReady {
        ifindex: u32,
    },
    /// Disconnected or key rotation failed.
    WifiLinkLost {
        ifindex: u32,
    },
    /// Kernel rfkill state for a Wi-Fi interface. `powered = true`
    /// ⇔ neither soft nor hard rfkill is asserted. Emitted by the
    /// `/dev/rfkill` watcher in `nexus-wifi::rfkill` on every
    /// `RFKILL_OP_ADD` / `RFKILL_OP_CHANGE` event that maps to a
    /// known wiphy, plus synthetic initial-state events on
    /// startup.
    WifiRfkillChanged {
        ifindex: u32,
        powered: bool,
    },
    /// wpa_supplicant asked for a credential (OTP, password,
    /// passphrase, smartcard PIN) to complete an ongoing
    /// authentication. The D-Bus layer surfaces this to operators
    /// so they can reply via a matching `WifiCommand::ProvideCredential`.
    /// See DD-003 §9.2 and DD-006 §9.
    WifiNetworkRequest {
        ifindex: u32,
        /// Opaque supplicant-side network path. Required round-trip
        /// payload for the reply command.
        network: String,
        /// Credential the supplicant wants (`password` / `otp` / …).
        field: String,
        /// Operator-facing prompt text.
        text: String,
    },

    // --- Bluetooth Backend ---
    /// Fires in two cases: (1) an adapter first becomes visible via
    /// BlueZ's ObjectManager `InterfacesAdded` (including the
    /// republish after a BlueZ reconnect), and (2) an existing
    /// adapter's `Powered` or `Discovering` property changed. The
    /// backend handler treats the event the same way in both cases —
    /// recompute the adapter's state machine from the
    /// `(powered, discovering)` tuple.
    BtAdapterChanged {
        /// e.g. `"/org/bluez/hciN"`.
        adapter: String,
        powered: bool,
        discovering: bool,
    },
    BtDeviceDiscovered(BtDeviceInfo),
    BtDeviceConnected {
        adapter: String,
        address: MacAddr,
    },
    BtDeviceDisconnected {
        adapter: String,
        address: MacAddr,
    },
    /// A pairing operation has started. Emitted when `Pair()` is
    /// called. The `PairingJobId` correlates subsequent
    /// `BtPairingPrompt` and `BtPairingComplete` events.
    BtPairingStarted {
        job_id: PairingJobId,
        device: String,
    },
    /// Emitted when BlueZ's Agent receives a callback that needs a
    /// human response (PIN, passkey, confirmation, incoming-connection
    /// authorization, etc.).
    BtPairingPrompt {
        job_id: PairingJobId,
        kind: PairingPromptKind,
        data: PairingPromptData,
    },
    /// Pairing finished. On success the device's state is `Paired`;
    /// on failure the reason is populated.
    BtPairingComplete {
        job_id: PairingJobId,
        success: bool,
        reason: Option<BtFailureReason>,
    },
    /// BlueZ D-Bus connection established (or re-established).
    BluezConnected,
    /// BlueZ D-Bus connection lost.
    BluezDisconnected,

    // --- GNSS Backend ---
    /// Raw TPV message from the gpsd client, before quality filtering
    /// or rate-capping. Consumers should prefer `GnssFixChanged`
    /// unless they specifically want unfiltered data.
    GnssTpvReceived {
        device: String,
        fix: GnssFix,
    },
    /// Filtered, rate-capped fix. Translated into
    /// `fi.nexus.Gnss.FixChanged` by the D-Bus layer.
    GnssFixChanged {
        device: String,
        fix: GnssFix,
    },
    GnssSatellites {
        device: String,
        satellites: Vec<SatInfo>,
    },
    /// The gpsd client has lost its connection to gpsd.
    GnssGpsdDisconnected,
    /// The gpsd client has (re)connected to gpsd.
    GnssGpsdConnected,
    /// Per-device lifecycle state transition (DD-005 §3.1). The
    /// `state` field carries the lower-case state name
    /// (`acquiring`, `tracking`, `degraded`, `gone`); the `reason`
    /// field captures the trigger (`fix_passed`, `fix_failed`,
    /// `timeout`, …) for diagnostic logs and tests.
    GnssStateChanged {
        device: String,
        from: &'static str,
        to: &'static str,
        reason: &'static str,
    },

    // --- Profile Store ---
    /// Put or remove completed.
    ProfileChanged {
        kind: ProfileKind,
        key: String,
    },
    /// Quarantined on load.
    ProfileCorrupt {
        kind: ProfileKind,
        key: String,
        reason: String,
    },
    /// Emitted during a master-key rotation at most every 10
    /// profiles or every 500 ms. Consumers can mirror the progress
    /// in operator UI.
    ProfileStoreRotationProgress {
        completed: u32,
        total: u32,
    },

    // --- Connectivity probe ---
    /// Internet-reachability state changed. Emitted by the
    /// connectivity probe (an HTTP/1.1 GET against a generate-204
    /// style endpoint; default `http://connectivity-check.ubuntu.com/`).
    /// The D-Bus layer mirrors this on
    /// `fi.nexus.Manager.InternetConnectivity` (read-only property)
    /// and `fi.nexus.Manager.InternetConnectivityChanged` (signal).
    /// Only fires on transitions, not on every successful probe.
    InternetConnectivityChanged {
        state: ConnectivityState,
    },

    // --- Any backend to the D-Bus layer ---
    /// Operator-facing notification. Translates to
    /// `fi.nexus.Manager.NotificationEvent` (DD-006 §5.3).
    OperatorNotification {
        kind: String,
        data: NotificationData,
    },
}

// ---------------------------------------------------------------------------
// Connectivity payload type
// ---------------------------------------------------------------------------

/// Outcome of an internet-reachability probe. Maps 1:1 onto the
/// `fi.nexus.Manager.Connectivity` D-Bus state strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConnectivityState {
    /// Pre-probe initial state. Surfaced once at startup before the
    /// first probe completes; never re-entered after the first
    /// transition.
    #[default]
    Unknown,
    /// The probe URL returned the expected `204 No Content`.
    Online,
    /// The probe URL responded but with a different status (typically
    /// `200` with an HTML login page, or a `3xx` redirect to one) —
    /// strong evidence of a captive portal in the path.
    CaptivePortal,
    /// DNS failure, connection refused, timeout, or any other
    /// transport-level failure.
    Offline,
}

impl ConnectivityState {
    /// Wire string consumed by `fi.nexus.Manager.Connectivity` and
    /// `fi.nexus.Manager.ConnectivityChanged`. Matches the values
    /// documented in the integration knowledge graph.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectivityState::Online => "internetOnline",
            ConnectivityState::CaptivePortal => "internetCaptivePortal",
            ConnectivityState::Offline => "internetOffline",
            ConnectivityState::Unknown => "internetUnknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Ethernet payload types (DD-002 §5.1)
// ---------------------------------------------------------------------------

/// 802.1X / wired-auth state surfaced by the `WiredAuthBackend`
/// implementation. See DD-002 §5.1.
#[derive(Debug, Clone)]
pub enum AuthState {
    /// Not yet authenticating (pre-attach, or after detach).
    Idle,
    Authenticating,
    Authenticated,
    Failed {
        reason: AuthFailureReason,
    },
}

/// Reason an `AuthState::Failed` was reached. See DD-002 §5.1.
#[derive(Debug, Clone)]
pub enum AuthFailureReason {
    BadCredentials,
    ServerUnreachable,
    CertificateRejected,
    Timeout,
    Other(String),
}

// ---------------------------------------------------------------------------
// Wi-Fi payload types (DD-003 §3.1, §4.2)
// ---------------------------------------------------------------------------

/// Wi-Fi per-interface state machine. See DD-003 §3.1
/// (`WifiInterfaceState`; the architecture-level alias `WifiState` is
/// the name used on the event bus).
#[derive(Debug, Clone)]
pub enum WifiState {
    /// Registered with supplicant, no network selected.
    Idle,
    /// Scan in progress.
    Scanning,
    /// Connection attempt in progress (pre-authentication).
    Connecting { bssid: MacAddr, ssid: Ssid },
    /// Authenticating (WPA2/WPA3 personal 4-way start, or EAP for
    /// Enterprise).
    Authenticating { bssid: MacAddr, ssid: Ssid },
    /// 4-way handshake in progress (WPA2/WPA3).
    Handshaking { bssid: MacAddr, ssid: Ssid },
    /// Connected, carrier up, associated. Ready for IP.
    Connected {
        bssid: MacAddr,
        ssid: Ssid,
        frequency: u32,
        signal_dbm: i32,
        security: SecurityMode,
    },
    /// Evaluating or executing a roam to a better BSS.
    Roaming {
        from: MacAddr,
        to: MacAddr,
        ssid: Ssid,
    },
    /// Disconnected with a specific reason.
    Disconnected { reason: DisconnectReason },
    /// Interface removed.
    Gone,
}

/// Security mode as advertised by a BSS or negotiated with one.
/// Distinct from a profile's required security config. See DD-003 §4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityMode {
    Open,
    Owe,
    /// Legacy; not connectable, just recognizable.
    Wep,
    Wpa2Psk,
    Wpa3Sae,
    /// AP advertises both PSK and SAE.
    Wpa2Wpa3Transition,
    Wpa2Eap,
    /// WPA-EAP-SHA256.
    Wpa3Eap,
    Wpa3EapSuiteB192,
}

/// Per-BSS 802.11 capability flags populated from scan result IEs.
/// See DD-003 §4.2.
#[derive(Debug, Clone, Default)]
pub struct BssCapabilities {
    /// 802.11n.
    pub ht: bool,
    /// 802.11ac.
    pub vht: bool,
    /// 802.11ax (Wi-Fi 6).
    pub he: bool,
    /// 802.11be (Wi-Fi 7).
    pub eht: bool,
    /// 802.11r Fast Transition.
    pub ft: bool,
    /// 802.11w MFPR bit.
    pub pmf_required: bool,
    /// 802.11w MFPC bit.
    pub pmf_capable: bool,
    /// WPS advertised (informational only).
    pub wps: bool,
}

/// Scan result for a single BSS. See DD-003 §4.2.
#[derive(Debug, Clone)]
pub struct BssInfo {
    pub bssid: MacAddr,
    pub ssid: Ssid,
    pub frequency: u32,
    pub signal_dbm: i32,
    pub capabilities: BssCapabilities,
    /// A BSS may advertise more than one.
    pub security: Vec<SecurityMode>,
    /// Time since last heard.
    pub age_ms: u64,
}

/// Reason for a `WifiState::Disconnected` transition. See DD-003 §4.2.
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    Unspecified,
    ApInitiated,
    AuthExpired,
    LocalRequest,
    Inactivity,
    ProtocolError,
    HandshakeTimeout,
    EapFailure,
    /// Permanent until operator intervention.
    CredentialsInvalid,
    /// rfkill asserted.
    RfKilled,
    SupplicantUnavailable,
    /// Transient; from wake.
    PostSleepRecovery,
    /// Detected per DD-003 §12.4.
    DriverWedge,
    Other(String),
}

impl DisconnectReason {
    /// True when the reason keeps the interface in `Disconnected`
    /// until operator intervention rather than cooling down to `Idle`.
    pub fn is_permanent(&self) -> bool {
        matches!(self, DisconnectReason::CredentialsInvalid)
    }
}

// ---------------------------------------------------------------------------
// Bluetooth payload types (DD-004 §6.2)
// ---------------------------------------------------------------------------

/// Snapshot of a device's current BlueZ-known properties. Emitted in
/// `NexusEvent::BtDeviceDiscovered` and on every property change.
#[derive(Debug, Clone)]
pub struct BtDeviceInfo {
    /// The adapter this device is scoped to, e.g. `"/org/bluez/hci0"`.
    pub adapter: String,
    /// BlueZ's object path for the device, e.g.
    /// `"/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"`.
    pub device_path: String,
    /// Bluetooth address.
    pub address: MacAddr,
    /// Address type as reported by BlueZ.
    pub address_type: BtAddressType,
    /// Friendly name from the device's GAP record, if resolved.
    pub name: Option<String>,
    /// Alias — BlueZ's editable local label. Equals `name` when
    /// unset.
    pub alias: Option<String>,
    /// RSSI in dBm, if recently observed.
    pub rssi: Option<i16>,
    /// Transmit power the peer is advertising (BLE only, optional).
    pub tx_power: Option<i16>,
    /// Service UUIDs (lowercase full-form).
    pub uuids: Vec<String>,
    /// Bluetooth transport synthesized by the backend from
    /// `address_type` + UUIDs at parse time.
    pub transport: BtTransport,
    /// Manufacturer data from GAP/advertisement: keyed by the IEEE
    /// manufacturer ID, value is the raw manufacturer-specific bytes.
    pub manufacturer_data: std::collections::HashMap<u16, Vec<u8>>,
    pub paired: bool,
    pub bonded: bool,
    pub trusted: bool,
    pub blocked: bool,
    pub connected: bool,
}

/// Address type as reported by BlueZ. See DD-004 §6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtAddressType {
    /// Classic BR/EDR public address.
    Bredr,
    /// BLE public identity address.
    LePublic,
    /// BLE random address (static random, RPA, or NRPA; BlueZ doesn't
    /// distinguish further).
    LeRandom,
}

/// Bluetooth transport derived by the backend. See DD-004 §6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtTransport {
    Bredr,
    Le,
    /// Device supports both (e.g., phone, laptop).
    Dual,
}

/// Correlation id for an in-flight pairing operation. See DD-004 §6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PairingJobId(pub Ulid);

/// Kind of Agent callback, mapped from BlueZ's Agent1 methods. See
/// DD-004 §6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingPromptKind {
    /// BlueZ invoked `RequestPinCode`.
    RequestPin,
    /// BlueZ invoked `RequestPasskey`.
    RequestPasskey,
    /// BlueZ invoked `DisplayPasskey`.
    DisplayPasskey,
    /// BlueZ invoked `DisplayPinCode`.
    DisplayPin,
    /// BlueZ invoked `RequestConfirmation`.
    RequestConfirmation,
    /// BlueZ invoked `RequestAuthorization`.
    RequestAuthorization,
    /// BlueZ invoked `AuthorizeService`.
    AuthorizeService,
}

/// Data accompanying a [`PairingPromptKind`]. Each kind populates a
/// different subset; unused fields are `None`. See DD-004 §6.2.
#[derive(Debug, Clone)]
pub struct PairingPromptData {
    /// BlueZ device path the prompt is about.
    pub device_path: String,
    /// Passkey to display or confirm. Always 000000..999999 for
    /// `DisplayPasskey` / `DisplayPin` / `RequestConfirmation`.
    pub passkey: Option<u32>,
    /// PIN code to display (`DisplayPin` only). Strings rather than
    /// `u32` because legacy PINs can be 4-16 printable ASCII chars.
    pub pincode: Option<String>,
    /// Service UUID being authorized (`AuthorizeService` only).
    pub service_uuid: Option<String>,
}

/// Operator response to a pairing prompt. Flows from the D-Bus
/// `AnswerPairingPrompt` method into the Bluetooth backend, which
/// validates the variant against the pending `PairingPromptKind`
/// (see DD-006 §6.4's per-kind variant map) before resolving the
/// Agent's pending oneshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingAnswer {
    /// PIN entered by the operator (`RequestPin`). 4-16 ASCII chars.
    Pin(String),
    /// Passkey entered by the operator (`RequestPasskey`).
    /// 0..=999_999 per the Bluetooth spec.
    Passkey(u32),
    /// Yes/no for any confirmation-style prompt (`RequestConfirmation`,
    /// `RequestAuthorization`, `AuthorizeService`).
    Accept(bool),
    /// The operator has seen a notification-only prompt
    /// (`DisplayPasskey` / `DisplayPin`). Tells the Agent it can
    /// return to BlueZ now.
    Acknowledge,
    /// Operator cancelled — maps to DD-006 §6.4's universal
    /// `"s":"cancel"` answer. Backend fails the pairing with reason
    /// "rejected".
    Cancel,
}

/// Reason for a Bluetooth pairing/connection failure. See DD-004 §5.1.
#[derive(Debug, Clone)]
pub enum BtFailureReason {
    /// Local or peer rejected.
    PairingRejected,
    PairingTimeout,
    /// PIN/passkey mismatch.
    PairingAuthFailed,
    /// Peer unreachable or BlueZ error.
    ConnectionFailed,
    /// BlueZ error text we don't map explicitly.
    Unknown(String),
}

// ---------------------------------------------------------------------------
// GNSS payload types (DD-005)
// ---------------------------------------------------------------------------

/// A single position/velocity/time fix. See DD-005.
#[derive(Debug, Clone)]
pub struct GnssFix {
    /// Receiver timestamp, satellite-derived UTC (falls back to wall
    /// clock if gpsd's time field was unparseable).
    pub time: DateTime<Utc>,
    /// Fix dimensionality and confidence.
    pub mode: FixMode,
    /// Degrees, WGS84. Present for `Fix2D` and `Fix3D`.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Meters, `altHAE` or `altMSL` as gpsd reports it. Present for
    /// `Fix3D`.
    pub altitude_m: Option<f64>,
    /// Meters per second over ground. Zero when stationary or
    /// unknown.
    pub speed_mps: Option<f64>,
    /// Degrees true, 0..360. Typically `None` when stationary.
    pub track_deg: Option<f64>,
    /// Estimated horizontal position error, meters (95 % CI).
    pub horizontal_error_m: Option<f64>,
    /// Estimated vertical error, meters (95 % CI).
    pub vertical_error_m: Option<f64>,
    /// Count of satellites used in the fix (not merely in view).
    pub satellites_used: u32,
}

/// Fix dimensionality as reported by gpsd. See DD-005.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixMode {
    /// gpsd mode 0 or 1.
    NoFix,
    /// gpsd mode 2.
    Fix2D,
    /// gpsd mode 3.
    Fix3D,
}

/// Per-satellite information from gpsd's SKY message. See DD-005.
#[derive(Debug, Clone)]
pub struct SatInfo {
    /// gpsd's `gnssid`: 0=GPS, 1=SBAS, 2=Galileo, 3=BeiDou, 5=QZSS,
    /// 6=GLONASS, 7=IRNSS.
    pub gnss_id: u8,
    /// Satellite identifier within its constellation (PRN).
    pub sv_id: u16,
    /// Signal-to-noise ratio, dB-Hz. Typical good values 30-50.
    pub snr_db: Option<f32>,
    /// Elevation above horizon, degrees.
    pub elevation_deg: Option<f32>,
    /// Azimuth from true north, degrees.
    pub azimuth_deg: Option<f32>,
    /// Whether this satellite contributed to the most recent fix.
    pub used: bool,
}

// ---------------------------------------------------------------------------
// Profile Store payload types (DD-007 §5.1)
// ---------------------------------------------------------------------------

/// Which profile family a [`NexusEvent::ProfileChanged`] or
/// [`NexusEvent::ProfileCorrupt`] refers to. See DD-007 §5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileKind {
    Ethernet,
    Wifi,
    Gnss,
    Bluetooth,
}
