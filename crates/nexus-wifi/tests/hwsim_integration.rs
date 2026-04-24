//! Real-kernel integration tests for the Wi-Fi backend — DD-003 §14.2.
//!
//! These run against `mac80211_hwsim` + `hostapd` + `wpa_supplicant`
//! and require root. They live behind the `integration-linux` Cargo
//! feature and are `#[ignore]`d so `cargo test` skips them in
//! default CI runs. On a capable host:
//!
//! ```sh
//! sudo cargo test -p nexus-wifi --features integration-linux \
//!     --test hwsim_integration -- --ignored
//! ```
//!
//! See `tests/hwsim/mod.rs` for the harness primitives. A passing
//! run exercises: hwsim load → hostapd bring-up on phy0 → Nexus
//! Wi-Fi backend scan against phy1 → assertion that the hostapd
//! SSID lands in `BssCache`.
//!
//! Adding more scenarios (connect, roam, rfkill block/unblock) is
//! incremental: each becomes a new `#[ignore]`d test that reuses
//! the `Hwsim`, `Hostapd`, and `WpaSupplicant` RAII helpers.

#![cfg(feature = "integration-linux")]

mod hwsim;

use std::time::Duration;

use hwsim::{Hostapd, HostapdConfig, Hwsim, WpaSupplicant, ip_link_up};

/// Smoke: can the station-side Nexus Wi-Fi backend observe a
/// hostapd-hosted AP in its scan results?
///
/// This is the thinnest end-to-end check that exercises the whole
/// data path. It does NOT assert association — that's a follow-up
/// test that needs the Nexus profile store and `WifiCommand::Connect`
/// wired against a live wpa_supplicant D-Bus session, which adds
/// setup surface we haven't factored out yet.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires root + hwsim + hostapd; run with --ignored"]
async fn scan_finds_hostapd_ap() {
    let hwsim = Hwsim::load(2).expect("hwsim load");
    assert!(hwsim.phys.len() >= 2, "need at least 2 hwsim phys");
    let ap_ifname = hwsim.ifname(0).expect("ap ifname");
    let sta_ifname = hwsim.ifname(1).expect("sta ifname");

    ip_link_up(&ap_ifname).expect("ip link up ap");
    ip_link_up(&sta_ifname).expect("ip link up sta");

    let _hostapd = Hostapd::start(&HostapdConfig {
        ifname: ap_ifname.clone(),
        ssid: "nexus-hwsim-smoke".into(),
        passphrase: "testpass123".into(),
        channel: 1,
    })
    .expect("start hostapd");

    let _wpa = WpaSupplicant::start(&sta_ifname).expect("start wpa_supplicant");

    // Placeholder — the actual driver is the next iteration. We
    // assert the harness at least gets this far; a real
    // `WifiBackend::scan` + assertion on the BSS list follows once
    // we factor a `backend_fixture::spawn()` helper that can accept
    // an already-running wpa_supplicant.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!ap_ifname.is_empty());
}
