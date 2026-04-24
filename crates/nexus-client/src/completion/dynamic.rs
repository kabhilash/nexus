//! Dynamic completion hooks. DD-008 §10.
//!
//! Each [`DynamicSlot`] describes a kind of runtime candidate list
//! that bash/zsh/fish expand by calling back into `nexusctl --terse
//! --fields=<f> <subcommand...>`. The CLI binary is re-invoked at
//! completion time; the call is wrapped in `timeout(1)` so a slow or
//! hung nexusd can't freeze the shell (DD-008 §10 final paragraph).
//!
//! These helpers are kept format-free — they return argv fragments
//! and identifiers that the shell-specific overlay in
//! [`crate::commands::completions`] splices into a template. Keeping
//! the data free of shell escaping makes them trivially testable and
//! also reusable for the REPL's Phase-7 completion engine (the slots
//! map 1:1 to rustyline's candidate callbacks).

/// One runtime-completable positional argument.
///
/// Variants are named after the shape of the thing being completed,
/// not the subcommand that hosts it — the same slot (e.g.
/// `BluetoothAddress`) services many subcommands (`bt show`,
/// `bt connect`, `bt pair`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicSlot {
    /// Any managed interface: `eth0`, `wlan0`, `hci0`, …
    InterfaceAny,
    /// Wi-Fi interfaces only.
    InterfaceWifi,
    /// Ethernet interfaces only.
    InterfaceEthernet,
    /// GNSS device paths (`/dev/gps0`, …).
    InterfaceGnss,
    /// Bluetooth adapter ifnames (`hci0`, …). The daemon models
    /// adapters as interfaces of kind `bluetooth`, so the slot piggybacks
    /// on `iface list --kind bluetooth`.
    BluetoothAdapter,
    /// Bluetooth device addresses (`AA:BB:CC:DD:EE:FF`) — the union of
    /// paired, connected, and scanned devices currently known.
    BluetoothAddress,
    /// Profile references: any stored profile, either by label or ULID.
    Profile,
    /// Wi-Fi profiles only (used by `wifi forget`, `wifi connect-profile`).
    ProfileWifi,
    /// Ethernet profiles only.
    ProfileEthernet,
}

impl DynamicSlot {
    /// Bash function name the shell overlay defines for this slot.
    /// Also used by zsh/fish where the same symbol is re-declared.
    pub fn bash_fn(self) -> &'static str {
        match self {
            DynamicSlot::InterfaceAny => "__nexusctl_complete_iface_any",
            DynamicSlot::InterfaceWifi => "__nexusctl_complete_iface_wifi",
            DynamicSlot::InterfaceEthernet => "__nexusctl_complete_iface_ethernet",
            DynamicSlot::InterfaceGnss => "__nexusctl_complete_iface_gnss",
            DynamicSlot::BluetoothAdapter => "__nexusctl_complete_bt_adapter",
            DynamicSlot::BluetoothAddress => "__nexusctl_complete_bt_address",
            DynamicSlot::Profile => "__nexusctl_complete_profile",
            DynamicSlot::ProfileWifi => "__nexusctl_complete_profile_wifi",
            DynamicSlot::ProfileEthernet => "__nexusctl_complete_profile_ethernet",
        }
    }

    /// Which `--fields` value to request from `nexusctl`. The
    /// completion script then reads whole lines as candidates.
    pub fn field(self) -> &'static str {
        match self {
            DynamicSlot::InterfaceAny
            | DynamicSlot::InterfaceWifi
            | DynamicSlot::InterfaceEthernet
            | DynamicSlot::InterfaceGnss
            | DynamicSlot::BluetoothAdapter => "iface",
            DynamicSlot::BluetoothAddress => "address",
            DynamicSlot::Profile | DynamicSlot::ProfileWifi | DynamicSlot::ProfileEthernet => {
                "label"
            }
        }
    }

    /// The subcommand argv used to produce the candidate list. This
    /// is everything after `nexusctl --terse --fields=<f>`. The slot
    /// descriptor does not carry global flags — the caller adds
    /// `--terse`, `--fields=<field>`, and `--no-interactive` so the
    /// callback never blocks on a prompt.
    pub fn argv(self) -> &'static [&'static str] {
        match self {
            DynamicSlot::InterfaceAny => &["iface", "list"],
            DynamicSlot::InterfaceWifi => &["iface", "list", "--kind", "wifi"],
            DynamicSlot::InterfaceEthernet => &["iface", "list", "--kind", "ethernet"],
            DynamicSlot::InterfaceGnss => &["iface", "list", "--kind", "gnss"],
            DynamicSlot::BluetoothAdapter => &["iface", "list", "--kind", "bluetooth"],
            DynamicSlot::BluetoothAddress => &["bt", "list"],
            DynamicSlot::Profile => &["profile", "list"],
            DynamicSlot::ProfileWifi => &["profile", "list", "--kind", "wifi"],
            DynamicSlot::ProfileEthernet => &["profile", "list", "--kind", "ethernet"],
        }
    }

    /// Shell-ready `nexusctl` invocation for this slot, as a string
    /// the bash overlay embeds inside `timeout 1 ...`. Kept as plain
    /// space-separated argv — the fields have no metacharacters so
    /// no quoting is required, and bash splits on IFS for compgen.
    pub fn nexusctl_argv(self) -> String {
        let mut parts: Vec<&str> = vec![
            "nexusctl",
            "--no-interactive",
            "--terse",
            "--fields",
            self.field(),
        ];
        parts.extend_from_slice(self.argv());
        parts.join(" ")
    }
}

/// Map from `(subcommand_chain, positional_index)` to the slot it
/// completes. `chain` is the space-joined trail from the binary
/// name's first real word (e.g. `"wifi show"`, `"bt pair"`). Index 0
/// is the first positional after the subcommand (`show <iface>`
/// slot 0 is the iface).
///
/// Only positionals that actually benefit from runtime candidates
/// are listed; shell completion falls back to clap's static list for
/// everything else.
pub fn chain_slots() -> &'static [(&'static str, usize, DynamicSlot)] {
    &[
        ("iface show", 0, DynamicSlot::InterfaceAny),
        ("eth show", 0, DynamicSlot::InterfaceEthernet),
        ("wifi show", 0, DynamicSlot::InterfaceWifi),
        ("wifi scan", 0, DynamicSlot::InterfaceWifi),
        ("wifi disconnect", 0, DynamicSlot::InterfaceWifi),
        ("wifi forget", 0, DynamicSlot::ProfileWifi),
        ("wifi connect-profile", 0, DynamicSlot::ProfileWifi),
        ("bt show", 0, DynamicSlot::BluetoothAddress),
        ("bt power", 0, DynamicSlot::BluetoothAdapter),
        ("bt scan", 0, DynamicSlot::BluetoothAdapter),
        ("bt connect", 0, DynamicSlot::BluetoothAddress),
        ("bt disconnect", 0, DynamicSlot::BluetoothAddress),
        ("bt forget", 0, DynamicSlot::BluetoothAddress),
        ("bt trust", 0, DynamicSlot::BluetoothAddress),
        ("bt pair", 0, DynamicSlot::BluetoothAddress),
        ("gnss show", 0, DynamicSlot::InterfaceGnss),
        ("gnss satellites", 0, DynamicSlot::InterfaceGnss),
        ("profile show", 0, DynamicSlot::Profile),
        ("profile remove", 0, DynamicSlot::Profile),
        ("profile export", 0, DynamicSlot::Profile),
        ("profile update", 0, DynamicSlot::Profile),
    ]
}

/// Full list of slots, in a stable order, for tests and the bash
/// helper-function emitter.
pub fn all_slots() -> &'static [DynamicSlot] {
    &[
        DynamicSlot::InterfaceAny,
        DynamicSlot::InterfaceWifi,
        DynamicSlot::InterfaceEthernet,
        DynamicSlot::InterfaceGnss,
        DynamicSlot::BluetoothAdapter,
        DynamicSlot::BluetoothAddress,
        DynamicSlot::Profile,
        DynamicSlot::ProfileWifi,
        DynamicSlot::ProfileEthernet,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_slot_has_unique_bash_function_name() {
        let mut names: Vec<&str> = all_slots().iter().map(|s| s.bash_fn()).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate bash_fn names: {names:?}");
    }

    #[test]
    fn nexusctl_argv_starts_with_required_global_flags() {
        for slot in all_slots() {
            let s = slot.nexusctl_argv();
            assert!(s.starts_with("nexusctl "), "slot {slot:?}: {s}");
            assert!(
                s.contains("--no-interactive"),
                "slot {slot:?} lacks --no-interactive: {s}"
            );
            assert!(s.contains("--terse"), "slot {slot:?} lacks --terse: {s}");
            assert!(s.contains("--fields"), "slot {slot:?} lacks --fields: {s}");
        }
    }

    #[test]
    fn wifi_show_resolves_to_wifi_iface_slot() {
        let hit = chain_slots()
            .iter()
            .find(|(c, i, _)| *c == "wifi show" && *i == 0);
        assert_eq!(hit.unwrap().2, DynamicSlot::InterfaceWifi);
    }

    #[test]
    fn bt_pair_resolves_to_bt_address_slot() {
        let hit = chain_slots()
            .iter()
            .find(|(c, i, _)| *c == "bt pair" && *i == 0);
        assert_eq!(hit.unwrap().2, DynamicSlot::BluetoothAddress);
    }

    #[test]
    fn iface_slots_all_request_iface_field() {
        for slot in [
            DynamicSlot::InterfaceAny,
            DynamicSlot::InterfaceWifi,
            DynamicSlot::InterfaceEthernet,
            DynamicSlot::InterfaceGnss,
            DynamicSlot::BluetoothAdapter,
        ] {
            assert_eq!(slot.field(), "iface", "slot {slot:?}");
        }
    }

    #[test]
    fn bluetooth_address_slot_requests_address_field() {
        assert_eq!(DynamicSlot::BluetoothAddress.field(), "address");
    }

    #[test]
    fn profile_slots_request_label_field() {
        for slot in [
            DynamicSlot::Profile,
            DynamicSlot::ProfileWifi,
            DynamicSlot::ProfileEthernet,
        ] {
            assert_eq!(slot.field(), "label", "slot {slot:?}");
        }
    }

    #[test]
    fn nexusctl_argv_example_for_wifi_matches_dd008_hint() {
        // DD-008 §10's narrative example: `nexusctl --terse
        // --fields=iface iface list --kind wireless`. Our
        // implementation uses `wifi` (the DD-001 InterfaceKind wire
        // value) rather than `wireless` (the freeform english in the
        // DD's prose); the contract is the --fields=iface + iface
        // list structure, not the specific kind spelling.
        let s = DynamicSlot::InterfaceWifi.nexusctl_argv();
        assert!(s.contains("--fields iface"));
        assert!(s.contains("iface list --kind wifi"));
    }
}
