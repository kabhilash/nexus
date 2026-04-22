pub mod netlink;

use nexus_core::{InterfaceInfo, MacAddr, NexusEvent};

pub fn placeholder() {
    let _ = std::mem::size_of::<(NexusEvent, InterfaceInfo, MacAddr)>();
}
