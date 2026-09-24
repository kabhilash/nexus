//! Metrics emitted by the Interface Monitor. Uses the [`metrics`]
//! crate facade — the real transport (OpenMetrics endpoint, OTLP,
//! etc.) is installed by `nexus-daemon` / `nexus-dbus` at startup.
//!
//! The authoritative metric list is fixed. Do not add new metrics
//! without updating DD-001 §9.5 alongside.

use metrics::{
    Unit, counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram,
};
use nexus_core::{InterfaceInfo, InterfaceKind};

/// Metric name constants. Exposed so tests and downstream consumers
/// can assert against the same identifiers.
pub const EVENTS_TOTAL: &str = "nexus_interface_events_total";
pub const DISCOVERY_DURATION: &str = "nexus_interface_discovery_duration_seconds";
pub const INTERFACE_COUNT: &str = "nexus_interface_count";
pub const ERRORS_TOTAL: &str = "nexus_interface_errors_total";

/// Error-source labels used on [`ERRORS_TOTAL`].
pub mod error_source {
    pub const RTNL_PARSE: &str = "rtnl_parse";
    pub const NL80211_PARSE: &str = "nl80211_parse";
    pub const NL80211_MCAST_RECV: &str = "nl80211_mcast_recv";
    pub const RTNL_RECV: &str = "rtnl_recv";
    pub const RTNL_ENOBUFS: &str = "rtnl_enobufs";
    pub const NL80211_ENOBUFS: &str = "nl80211_enobufs";
    pub const CLASSIFY_TIMEOUT: &str = "classify_timeout";
    pub const UDEV_ENUMERATE: &str = "udev_enumerate";
}

/// Kind-of-event labels used on [`EVENTS_TOTAL`].
pub mod event_label {
    pub const INTERFACE_DISCOVERED: &str = "interface_discovered";
    pub const INTERFACE_REMOVED: &str = "interface_removed";
    pub const CARRIER_CHANGED: &str = "carrier_changed";
    pub const OPERSTATE_CHANGED: &str = "operstate_changed";
    pub const MAC_CHANGED: &str = "mac_changed";
}

/// Describe every metric with its unit and help text. Safe to call
/// more than once; the `metrics` crate deduplicates description
/// updates.
pub fn register() {
    describe_counter!(
        EVENTS_TOTAL,
        "Interface-monitor events emitted on the NexusEvent bus, by interface kind and event type"
    );
    describe_histogram!(
        DISCOVERY_DURATION,
        Unit::Seconds,
        "Time from monitor startup to cold-boot enumeration completion"
    );
    describe_gauge!(
        INTERFACE_COUNT,
        "Current number of registered interfaces, by kind"
    );
    describe_counter!(
        ERRORS_TOTAL,
        "Interface-monitor error occurrences, by error source"
    );
}

/// Short string identifier for an [`InterfaceKind`], used as the
/// `kind` label value.
pub fn kind_label(kind: &InterfaceKind) -> &'static str {
    match kind {
        InterfaceKind::Ethernet => "ethernet",
        InterfaceKind::Wireless { .. } => "wireless",
        InterfaceKind::Bluetooth { .. } => "bluetooth",
        InterfaceKind::Gnss { .. } => "gnss",
    }
}

/// Record one NexusEvent emission on [`EVENTS_TOTAL`].
pub fn record_event(kind: &str, event: &str) {
    counter!(EVENTS_TOTAL, "kind" => kind.to_owned(), "event" => event.to_owned()).increment(1);
}

/// Convenience wrapper: [`kind_label`] + [`record_event`].
pub fn record_event_for(info: &InterfaceInfo, event: &str) {
    record_event(kind_label(&info.kind), event);
}

/// Record the cold-boot duration.
pub fn record_discovery_duration(secs: f64) {
    histogram!(DISCOVERY_DURATION).record(secs);
}

/// Set the current registered-interface gauge for one kind.
pub fn set_interface_count(kind: &str, count: u64) {
    gauge!(INTERFACE_COUNT, "kind" => kind.to_owned()).set(count as f64);
}

/// Refresh [`INTERFACE_COUNT`] gauges for every kind from the given
/// registry. Emits zero for kinds that have no current members so
/// deletions are observable.
pub fn refresh_interface_counts<'a, I>(infos: I)
where
    I: IntoIterator<Item = &'a InterfaceInfo>,
{
    let mut counts = [0u64; 4];
    for info in infos {
        let idx = match info.kind {
            InterfaceKind::Ethernet => 0,
            InterfaceKind::Wireless { .. } => 1,
            InterfaceKind::Bluetooth { .. } => 2,
            InterfaceKind::Gnss { .. } => 3,
        };
        counts[idx] += 1;
    }
    set_interface_count("ethernet", counts[0]);
    set_interface_count("wireless", counts[1]);
    set_interface_count("bluetooth", counts[2]);
    set_interface_count("gnss", counts[3]);
}

/// Record one error occurrence on [`ERRORS_TOTAL`].
pub fn record_error(source: &str) {
    counter!(ERRORS_TOTAL, "source" => source.to_owned()).increment(1);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use nexus_core::{
        InterfaceInfo, InterfaceKind, MacAddr, Nl80211IfType, OperState, PhyCapabilities,
    };

    use super::*;

    fn ethernet(ifindex: u32) -> InterfaceInfo {
        InterfaceInfo {
            ifindex,
            ifname: format!("eth{ifindex}"),
            mac: [0; 6],
            mtu: 1500,
            operstate: OperState::Up,
            carrier: true,
            kind: InterfaceKind::Ethernet,
            discovered_at: Instant::now(),
        }
    }

    fn wireless(ifindex: u32) -> InterfaceInfo {
        InterfaceInfo {
            ifindex,
            ifname: format!("wlan{ifindex}"),
            mac: [0; 6],
            mtu: 1500,
            operstate: OperState::Up,
            carrier: true,
            kind: InterfaceKind::Wireless {
                wiphy: 0,
                wiphy_name: "phy0".into(),
                wdev: 1,
                iftype: Nl80211IfType(2),
                capabilities: Arc::new(PhyCapabilities::default()),
            },
            discovered_at: Instant::now(),
        }
    }

    fn bluetooth(hci: u32) -> InterfaceInfo {
        InterfaceInfo {
            ifindex: 0x8000_0000 | hci,
            ifname: format!("hci{hci}"),
            mac: [0; 6],
            mtu: 0,
            operstate: OperState::Up,
            carrier: true,
            kind: InterfaceKind::Bluetooth {
                hci_name: format!("hci{hci}"),
                hci_index: hci,
                bt_address: MacAddr([0; 6]),
                bluez_path: format!("/org/bluez/hci{hci}"),
            },
            discovered_at: Instant::now(),
        }
    }

    #[test]
    fn kind_label_covers_every_variant() {
        assert_eq!(kind_label(&InterfaceKind::Ethernet), "ethernet");
        match &wireless(1).kind {
            k @ InterfaceKind::Wireless { .. } => assert_eq!(kind_label(k), "wireless"),
            _ => unreachable!(),
        }
        match &bluetooth(0).kind {
            k @ InterfaceKind::Bluetooth { .. } => assert_eq!(kind_label(k), "bluetooth"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn register_is_idempotent() {
        register();
        register();
        // No assertion beyond "didn't panic".
    }

    #[test]
    fn refresh_interface_counts_does_not_panic_on_empty_or_mixed() {
        register();
        refresh_interface_counts(std::iter::empty());
        let infos = [ethernet(2), ethernet(3), wireless(4), bluetooth(0)];
        refresh_interface_counts(infos.iter());
    }

    #[test]
    fn recording_helpers_do_not_panic() {
        register();
        record_event("ethernet", event_label::INTERFACE_DISCOVERED);
        record_discovery_duration(0.042);
        record_error(error_source::RTNL_PARSE);
    }
}
