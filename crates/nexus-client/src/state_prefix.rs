//! Per-interface state-prefix column (DD-008 §5.1).
//!
//! Produces a 3-character badge like `*O `, `*AO`, or `*! ` that
//! gives a connmanctl-style at-a-glance state read in the human
//! list output.
//!
//! Codes:
//! - `*` — interface present and enabled
//! - `A` — auto-configured / auto-connected (profile in use)
//! - `O` — online (carrier up, state up/connected)
//! - `R` — ready / powered but not actively connected (BT adapters)
//! - `F` — has a fix (GNSS)
//! - `!` — error state — takes precedence over every other letter
//!
//! # What's computable from Phase 1's data
//!
//! Phase 1's [`InterfaceSummary`] carries ifname/kind/state/mac/
//! carrier. That's enough for `*`, `O`, `R`, `F`, and `!`. `A`
//! requires profile-attachment data that the per-kind interfaces
//! expose via their Wi-Fi/Ethernet/Bluetooth properties — those
//! land in Phase 3 when [`crate::proxy::ManagerOps`] grows
//! per-kind getters, at which point [`classify`] picks them up.

use crate::proxy::InterfaceSummary;

/// Compact state badge for a single interface row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePrefix {
    pub present: bool,
    pub auto: bool,
    pub online: bool,
    pub ready: bool,
    pub fix: bool,
    pub error: bool,
}

impl StatePrefix {
    /// Render as the exact 3-character string DD-008 §5.1's example
    /// output shows (e.g. `"*O "`, `"*AO"`, `"*R "`, `"*! "`,
    /// `"   "` when nothing applies). Callers either prepend this
    /// or feed it to a comfy-table cell.
    pub fn render(&self) -> String {
        if !self.present {
            return "   ".to_owned();
        }
        let mut out = String::from("*");
        if self.error {
            // DD-008 §5.1 says `!` takes precedence over every
            // other letter. Render `*! `.
            out.push('!');
            out.push(' ');
            return out;
        }
        if self.auto {
            out.push('A');
        }
        if self.online {
            out.push('O');
        } else if self.ready {
            out.push('R');
        } else if self.fix {
            out.push('F');
        }
        // Pad to width 3. Keeps column alignment in tables.
        while out.len() < 3 {
            out.push(' ');
        }
        out
    }
}

/// Produce a badge for an interface summary.
///
/// Per-kind logic lives here rather than on the row type itself
/// so `InterfaceSummary` stays a plain data shape, and so the
/// rules are inspectable + unit-testable in one place.
pub fn classify(row: &InterfaceSummary) -> StatePrefix {
    let kind = row.kind.as_str();
    let state = row.state.to_ascii_lowercase();

    // Rows with no prefix (per DD-008 §5.1 "interfaces Nexus knows
    // about but hasn't activated"): operstate is notpresent /
    // unknown, or the bluetooth adapter hasn't moved past
    // Unavailable. The "hidden" prefix shows up for down interfaces
    // in practice.
    let present = !matches!(
        state.as_str(),
        "" | "notpresent" | "unknown" | "unavailable"
    );

    // `!` — an unrecoverable-looking state. Each kind spells its
    // failures slightly differently; we accept "fail" as a
    // substring plus well-known terminal names.
    let error = state.contains("fail") || state == "auth_failed" || state == "gone";

    // `O` / `R` / `F` — per-kind classification.
    let mut online = false;
    let mut ready = false;
    let mut fix = false;

    match kind {
        "ethernet" => {
            // Ethernet: Up with a carrier up is online. State
            // "authenticated" / "link_ready" also count as online
            // per DD-002's auth state machine.
            online = row.carrier && matches!(state.as_str(), "up" | "authenticated" | "link_ready");
        }
        "wifi" | "wireless" => {
            // Wi-Fi: Connected with a carrier is online. DD-001's
            // kernel-derived `oper_state` exposes these as strings.
            online = matches!(state.as_str(), "connected" | "up");
        }
        "bluetooth" => {
            // BT adapter: Powered → `R`, Discovering → `R` (not a
            // device connection), anything past `R` that implies a
            // connected device bumps to online — but device-level
            // detail isn't in the adapter summary yet, so Phase 2
            // stops at `R`.
            ready = matches!(state.as_str(), "powered" | "discovering");
        }
        "gnss" => {
            // GNSS is the odd one — "tracking" and "fix" are
            // what the gpsd state machine emits when position is
            // available.
            fix = matches!(state.as_str(), "tracking" | "fix");
        }
        _ => {}
    }

    // `A` — profile attached (ManagedProfile points at a real
    // profile object; `/` means none). Phase 3 plumbs this via
    // `InterfaceSummary::managed_profile`.
    let auto = row.managed_profile.is_some();

    StatePrefix {
        present,
        auto,
        online,
        ready,
        fix,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, state: &str, carrier: bool) -> InterfaceSummary {
        InterfaceSummary {
            iface: "dummy".into(),
            kind: kind.into(),
            state: state.into(),
            mac: None,
            carrier,
            managed_profile: None,
        }
    }

    #[test]
    fn ethernet_up_with_carrier_is_star_o() {
        let p = classify(&row("ethernet", "up", true));
        assert_eq!(p.render(), "*O ");
    }

    #[test]
    fn ethernet_up_without_carrier_is_star_only() {
        let p = classify(&row("ethernet", "up", false));
        assert_eq!(p.render(), "*  ");
    }

    #[test]
    fn wifi_connected_is_star_o() {
        let p = classify(&row("wifi", "connected", true));
        assert_eq!(p.render(), "*O ");
    }

    #[test]
    fn wifi_dormant_is_star_only() {
        let p = classify(&row("wifi", "dormant", false));
        assert_eq!(p.render(), "*  ");
    }

    #[test]
    fn bt_powered_is_star_r() {
        let p = classify(&row("bluetooth", "powered", false));
        assert_eq!(p.render(), "*R ");
    }

    #[test]
    fn gnss_tracking_is_star_f() {
        let p = classify(&row("gnss", "tracking", false));
        assert_eq!(p.render(), "*F ");
    }

    #[test]
    fn notpresent_renders_empty() {
        let p = classify(&row("ethernet", "notpresent", false));
        assert_eq!(p.render(), "   ");
    }

    #[test]
    fn failure_state_takes_precedence_over_letters() {
        // auth_failed wins over any carrier/state letter.
        let p = classify(&row("ethernet", "auth_failed", true));
        assert_eq!(p.render(), "*! ");
    }

    #[test]
    fn down_ethernet_with_no_carrier_is_still_present_but_offline() {
        let p = classify(&row("ethernet", "down", false));
        assert_eq!(p.render(), "*  ");
    }

    #[test]
    fn managed_profile_sets_auto_flag() {
        let mut r = row("wifi", "connected", true);
        r.managed_profile = Some("/fi/nexus1/profile/wifi/X".into());
        let p = classify(&r);
        assert_eq!(p.render(), "*AO");
    }
}
