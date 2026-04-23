//! Profile helpers. The on-disk shape lives in
//! [`nexus_profile_store::EthernetProfile`]; this module just
//! provides the "no profile found" fallback DD-002 §3.3 mentions.

use nexus_profile_store::{Dot1xSettings, EthInterfaceSettings, EthernetProfile, ProfileMetadata};
use ulid::Ulid;

/// Build the default [`EthernetProfile`] used when the Profile
/// Store has no record for an interface. It has 802.1X disabled
/// and `auto_connect = true`, matching the "just track this
/// interface" behavior DD-002 expects for consumer/home Ethernet.
pub fn default_ethernet_profile(ifname: &str) -> EthernetProfile {
    EthernetProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        interface: EthInterfaceSettings {
            name: ifname.to_owned(),
            auto_connect: true,
        },
        dot1x: None,
    }
}

/// True when the profile asks the backend to drive 802.1X.
pub fn profile_requires_auth(profile: &EthernetProfile) -> bool {
    match &profile.dot1x {
        Some(Dot1xSettings { enabled, .. }) => *enabled,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_has_no_dot1x() {
        let p = default_ethernet_profile("eth0");
        assert_eq!(p.interface.name, "eth0");
        assert!(p.interface.auto_connect);
        assert!(p.dot1x.is_none());
        assert!(!profile_requires_auth(&p));
    }
}
