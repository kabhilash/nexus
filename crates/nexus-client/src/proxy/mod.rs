//! D-Bus proxy layer (DD-008 §7.2).
//!
//! Two things live here:
//!
//! 1. [`ManagerOps`] — an async trait that captures every operation
//!    Phase 1 commands need against `fi.nexus.Manager` and the per-
//!    interface `fi.nexus.Interface` objects. Command handlers
//!    depend on the trait, not on a concrete zbus proxy, so unit
//!    tests can pass a hand-rolled mock without spinning up a bus.
//!
//! 2. [`zbus_ops::ZbusManagerOps`] — the production impl that
//!    actually talks to nexusd. It wraps the generated zbus proxies
//!    in [`manager::ManagerProxy`] and [`interface::InterfaceProxy`].

pub mod interface;
pub mod manager;
pub mod zbus_ops;

use async_trait::async_trait;
use serde::Serialize;

use crate::errors::NexusctlError;

pub use zbus_ops::ZbusManagerOps;

/// Snapshot of the daemon's overall state. Mirrors the
/// `Manager.GetManagerStatus()` `a{sv}` shape from DD-006 §5.2 with
/// the fields nexusctl renders.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ManagerStatus {
    pub version: String,
    pub power_state: String,
    pub api_capabilities: Vec<String>,
    pub interface_count: u32,
    pub wifi_profile_count: u32,
    pub ethernet_profile_count: u32,
    pub bluetooth_profile_count: u32,
    pub master_key_source: String,
}

/// One row in `nexusctl iface list`. A summary of the common
/// `fi.nexus.Interface` properties — the per-kind detail columns
/// land in later phases.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InterfaceSummary {
    pub iface: String,
    pub kind: String,
    pub state: String,
    pub mac: Option<String>,
    pub carrier: bool,
}

/// Operations Phase 1 commands need. Production wires
/// [`ZbusManagerOps`]; tests wire a stub. The trait is intentionally
/// narrow — adding a method requires updating every impl and every
/// snapshot test.
#[async_trait]
pub trait ManagerOps: Send + Sync {
    async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError>;
    async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError>;
}
