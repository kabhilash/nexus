//! In-memory registry of every discovered interface, keyed by
//! `ifindex`. See DD-001 §5.5.
//!
//! For Ethernet and Wi-Fi the key is the real kernel `ifindex`. For
//! Bluetooth and GNSS it is a synthesized value (they have no kernel
//! ifindex). §5.4 fixes the top-bit ranges:
//!
//! | Kind      | Mask          | Low bits               |
//! |-----------|---------------|------------------------|
//! | Bluetooth | `0x8000_0000` | HCI index (`hciN`)     |
//! | GNSS      | `0x9000_0000` | Monitor-local sequence |

use std::collections::{HashMap, hash_map};

use nexus_core::InterfaceInfo;

/// Mask applied to `hci_index` to get a Bluetooth `ifindex`.
pub const BT_IFINDEX_MASK: u32 = 0x8000_0000;
/// Mask applied to the GNSS monitor-local sequence number.
pub const GNSS_IFINDEX_MASK: u32 = 0x9000_0000;

/// Synthesize an `ifindex` for a Bluetooth HCI adapter.
pub const fn bt_ifindex(hci_index: u32) -> u32 {
    BT_IFINDEX_MASK | hci_index
}

/// Synthesize an `ifindex` for a GNSS device.
pub const fn gnss_ifindex(seq: u32) -> u32 {
    GNSS_IFINDEX_MASK | seq
}

/// Was this `ifindex` synthesized for Bluetooth?
pub const fn is_bt_ifindex(ifindex: u32) -> bool {
    ifindex & BT_IFINDEX_MASK == BT_IFINDEX_MASK && ifindex & GNSS_IFINDEX_MASK != GNSS_IFINDEX_MASK
}

/// Was this `ifindex` synthesized for GNSS?
pub const fn is_gnss_ifindex(ifindex: u32) -> bool {
    ifindex & GNSS_IFINDEX_MASK == GNSS_IFINDEX_MASK
}

/// In-memory registry of discovered interfaces. Not thread-safe —
/// the monitor task is the sole writer.
#[derive(Debug, Default)]
pub struct Registry {
    by_ifindex: HashMap<u32, InterfaceInfo>,
    next_gnss_seq: u32,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace. Returns the previous record if one existed.
    pub fn insert(&mut self, info: InterfaceInfo) -> Option<InterfaceInfo> {
        self.by_ifindex.insert(info.ifindex, info)
    }

    /// Remove the interface with `ifindex`, returning the removed
    /// record.
    pub fn remove(&mut self, ifindex: u32) -> Option<InterfaceInfo> {
        self.by_ifindex.remove(&ifindex)
    }

    pub fn get(&self, ifindex: u32) -> Option<&InterfaceInfo> {
        self.by_ifindex.get(&ifindex)
    }

    pub fn get_mut(&mut self, ifindex: u32) -> Option<&mut InterfaceInfo> {
        self.by_ifindex.get_mut(&ifindex)
    }

    pub fn contains(&self, ifindex: u32) -> bool {
        self.by_ifindex.contains_key(&ifindex)
    }

    pub fn iter(&self) -> hash_map::Values<'_, u32, InterfaceInfo> {
        self.by_ifindex.values()
    }

    pub fn len(&self) -> usize {
        self.by_ifindex.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ifindex.is_empty()
    }

    /// Allocate and return the next GNSS ifindex. The monotonic
    /// counter is scoped to this registry instance.
    pub fn allocate_gnss_ifindex(&mut self) -> u32 {
        let seq = self.next_gnss_seq;
        self.next_gnss_seq += 1;
        gnss_ifindex(seq)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use nexus_core::{InterfaceInfo, InterfaceKind, OperState, PhyCapabilities};

    use super::*;

    fn ethernet(ifindex: u32, name: &str) -> InterfaceInfo {
        InterfaceInfo {
            ifindex,
            ifname: name.to_owned(),
            mac: [0; 6],
            mtu: 1500,
            operstate: OperState::Up,
            carrier: true,
            kind: InterfaceKind::Ethernet,
            discovered_at: Instant::now(),
        }
    }

    fn wireless(ifindex: u32, wiphy: u32) -> InterfaceInfo {
        InterfaceInfo {
            ifindex,
            ifname: format!("wlan{wiphy}"),
            mac: [0; 6],
            mtu: 1500,
            operstate: OperState::Up,
            carrier: true,
            kind: InterfaceKind::Wireless {
                wiphy,
                wiphy_name: format!("phy{wiphy}"),
                wdev: (u64::from(wiphy) << 32) | 1,
                iftype: nexus_core::Nl80211IfType(2),
                capabilities: Arc::new(PhyCapabilities::default()),
            },
            discovered_at: Instant::now(),
        }
    }

    #[test]
    fn bt_and_gnss_masks_do_not_overlap() {
        let bt = bt_ifindex(0);
        let gnss = gnss_ifindex(0);
        assert!(is_bt_ifindex(bt));
        assert!(!is_gnss_ifindex(bt));
        assert!(is_gnss_ifindex(gnss));
        assert!(!is_bt_ifindex(gnss));
    }

    #[test]
    fn bt_ifindex_masks_in_high_bit() {
        assert_eq!(bt_ifindex(0), 0x8000_0000);
        assert_eq!(bt_ifindex(3), 0x8000_0003);
    }

    #[test]
    fn gnss_ifindex_masks_in_the_0x9000_range() {
        assert_eq!(gnss_ifindex(0), 0x9000_0000);
        assert_eq!(gnss_ifindex(7), 0x9000_0007);
    }

    #[test]
    fn insert_and_remove_roundtrip() {
        let mut r = Registry::new();
        r.insert(ethernet(2, "eth0"));
        r.insert(ethernet(3, "eth1"));
        r.insert(wireless(4, 0));
        assert_eq!(r.len(), 3);
        assert!(r.contains(2));
        assert_eq!(r.get(3).unwrap().ifname, "eth1");

        let removed = r.remove(3).unwrap();
        assert_eq!(removed.ifname, "eth1");
        assert_eq!(r.len(), 2);
        assert!(!r.contains(3));
    }

    #[test]
    fn allocate_gnss_ifindex_is_monotonic() {
        let mut r = Registry::new();
        assert_eq!(r.allocate_gnss_ifindex(), gnss_ifindex(0));
        assert_eq!(r.allocate_gnss_ifindex(), gnss_ifindex(1));
        assert_eq!(r.allocate_gnss_ifindex(), gnss_ifindex(2));
    }
}
