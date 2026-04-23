//! `nexusd` — the Nexus daemon binary. See
//! `docs/nexus-architecture.md` §4-§5.
//!
//! The daemon itself is thin: it parses CLI args, loads a single
//! `nexus.toml`, fans out a shared `NexusEvent` broadcast channel,
//! spawns each enabled subsystem under a small supervisor, and
//! blocks on a `CancellationToken` driven by `SIGTERM` / `SIGINT`.
//! Subsystem bodies live in the per-technology crates (`nexus-wifi`,
//! `nexus-ethernet`, …); this crate only owns composition.
//!
//! Library surface: `config::Config`, `bus::spawn_bus`,
//! `supervision::spawn_supervised` — exposed so the daemon's
//! integration tests can exercise them without re-implementing the
//! glue. The `nexusd` binary wires them together in `main.rs`.

pub mod bus;
pub mod config;
pub mod reload;
pub mod supervision;

pub use bus::spawn_bus;
pub use config::{
    BluetoothSection, Config, ConfigError, DbusSection, EthernetSection, GnssSection,
    InterfaceMonitorSection, ProfileStoreSection, SupervisionSection, WifiSection,
};
pub use reload::{LogLevelSetter, ReloadCoordinator, ReloadError, ReloadOps, diff_config};
pub use supervision::{SubsystemName, SupervisionError, spawn_supervised};
