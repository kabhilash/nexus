//! Bluetooth Backend metrics. See DD-004 §13.2.

use ::metrics::{counter, describe_counter, describe_gauge, gauge};

pub const ADAPTERS: &str = "nexus_bluetooth_adapters";
pub const ADAPTER_STATE: &str = "nexus_bluetooth_adapter_state";
pub const DEVICES: &str = "nexus_bluetooth_devices";
pub const DEVICE_STATE: &str = "nexus_bluetooth_device_state";
pub const DISCOVERIES_TOTAL: &str = "nexus_bluetooth_discoveries_total";
pub const PAIRINGS_TOTAL: &str = "nexus_bluetooth_pairings_total";
pub const CONNECTIONS_TOTAL: &str = "nexus_bluetooth_connections_total";
pub const AGENT_CALLBACKS_TOTAL: &str = "nexus_bluetooth_agent_callbacks_total";
pub const BLUEZ_RECONNECTS_TOTAL: &str = "nexus_bluetooth_bluez_reconnects_total";
pub const BLUEZ_CONNECTED: &str = "nexus_bluetooth_bluez_connected";

pub mod connect_outcome {
    pub const SUCCESS: &str = "success";
    pub const FAILED: &str = "failed";
}

pub mod pair_outcome {
    pub const SUCCESS: &str = "success";
    pub const REJECTED: &str = "rejected";
    pub const TIMEOUT: &str = "timeout";
    pub const AUTH_FAILED: &str = "auth_failed";
    pub const OTHER: &str = "other";
}

pub fn register() {
    describe_gauge!(ADAPTERS, "Number of registered Bluetooth adapters");
    describe_gauge!(
        ADAPTER_STATE,
        "1 iff the adapter is currently in the labeled state"
    );
    describe_gauge!(DEVICES, "Devices currently known per adapter");
    describe_gauge!(DEVICE_STATE, "1 iff the device is in the labeled state");
    describe_counter!(DISCOVERIES_TOTAL, "Discovery sessions started");
    describe_counter!(PAIRINGS_TOTAL, "Pairing attempts by outcome");
    describe_counter!(CONNECTIONS_TOTAL, "Connection attempts by outcome");
    describe_counter!(AGENT_CALLBACKS_TOTAL, "Agent callbacks received by kind");
    describe_counter!(BLUEZ_RECONNECTS_TOTAL, "BlueZ connection attempts");
    describe_gauge!(
        BLUEZ_CONNECTED,
        "1 when BlueZ D-Bus connection is alive, 0 otherwise"
    );
}

pub fn set_bluez_connected(up: bool) {
    gauge!(BLUEZ_CONNECTED).set(if up { 1.0 } else { 0.0 });
}

pub fn record_bluez_reconnect() {
    counter!(BLUEZ_RECONNECTS_TOTAL).increment(1);
}

pub fn set_adapters(n: u64) {
    gauge!(ADAPTERS).set(n as f64);
}

pub fn set_adapter_state(adapter: &str, state: &str, active: bool) {
    gauge!(
        ADAPTER_STATE,
        "adapter" => adapter.to_owned(),
        "state" => state.to_owned(),
    )
    .set(if active { 1.0 } else { 0.0 });
}

pub fn set_devices(adapter: &str, n: u64) {
    gauge!(DEVICES, "adapter" => adapter.to_owned()).set(n as f64);
}

pub fn set_device_state(adapter: &str, address: &str, state: &str, active: bool) {
    gauge!(
        DEVICE_STATE,
        "adapter" => adapter.to_owned(),
        "address" => address.to_owned(),
        "state" => state.to_owned(),
    )
    .set(if active { 1.0 } else { 0.0 });
}

pub fn record_discovery(adapter: &str) {
    counter!(DISCOVERIES_TOTAL, "adapter" => adapter.to_owned()).increment(1);
}

pub fn record_connection(adapter: &str, outcome: &str) {
    counter!(
        CONNECTIONS_TOTAL,
        "adapter" => adapter.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

pub fn record_pairing(adapter: &str, outcome: &str) {
    counter!(
        PAIRINGS_TOTAL,
        "adapter" => adapter.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

pub fn record_agent_callback(kind: &str) {
    counter!(AGENT_CALLBACKS_TOTAL, "kind" => kind.to_owned()).increment(1);
}
