#![allow(dead_code)] // Helpers reserved for upcoming tests (connect, roam, rfkill).
//! Test harness for the mac80211_hwsim + hostapd + wpa_supplicant
//! integration scenario described in DD-003 §14.2.
//!
//! Everything here is `#[cfg(feature = "integration-linux")]`-gated
//! and requires root; the helpers shell out to stock Linux tooling
//! so they can't be exercised in an unprivileged devcontainer. On
//! a capable host:
//!
//! ```sh
//! sudo cargo test -p nexus-wifi --features integration-linux \
//!     --test hwsim_integration -- --ignored
//! ```
//!
//! Prerequisites:
//! - Root (kernel module loading + network namespaces / root-only
//!   wpa_supplicant control socket).
//! - `mac80211_hwsim`, `hostapd`, `wpa_supplicant`, `iw`, `ip`
//!   present in `$PATH`. All are in every mainstream distro's main
//!   repos.
//! - The running kernel must support `mac80211_hwsim` as a
//!   loadable module.
//!
//! Every helper uses RAII (`Drop`) cleanup so a panicked test
//! leaves the host in the same state it started in — no stray
//! hostapd, no loaded hwsim module, no orphaned wpa_supplicant
//! instance. The drops are best-effort; they `.ok()` every result
//! because by the time a Drop runs we can't propagate errors.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Wall-clock budget for each "wait for X to appear" helper. Hwsim
/// tends to register `phyN` within ~50 ms on modern kernels, so
/// 5 s is a generous cap that still fails fast on a broken run.
const APPEAR_DEADLINE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// mac80211_hwsim loader
// ---------------------------------------------------------------------------

/// Load `mac80211_hwsim` with `radios=N` and wait for the
/// corresponding `phyN` entries. Dropping unloads the module.
pub struct Hwsim {
    pub radios: u32,
    pub phys: Vec<String>,
}

impl Hwsim {
    pub fn load(radios: u32) -> std::io::Result<Self> {
        // `radios=` creates hwsim-managed wiphys; without it hwsim
        // defaults to 2 which is fine but we want to be explicit.
        let before = current_phys()?;
        let status = Command::new("modprobe")
            .args(["mac80211_hwsim", &format!("radios={radios}")])
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other("modprobe mac80211_hwsim failed"));
        }
        wait_until(APPEAR_DEADLINE, || {
            match current_phys() {
                Ok(now) => now.len() >= before.len() + radios as usize,
                Err(_) => false,
            }
        })
        .ok_or_else(|| std::io::Error::other("hwsim radios never registered"))?;
        let after = current_phys()?;
        let new_phys: Vec<String> = after
            .into_iter()
            .filter(|p| !before.contains(p))
            .collect();
        Ok(Self {
            radios,
            phys: new_phys,
        })
    }

    /// Interface name (`wlanN`) associated with `self.phys[idx]`.
    pub fn ifname(&self, idx: usize) -> std::io::Result<String> {
        let phy = &self.phys[idx];
        let net_dir = PathBuf::from(format!("/sys/class/ieee80211/{phy}/device/net"));
        let first = std::fs::read_dir(net_dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .next()
            .ok_or_else(|| std::io::Error::other(format!("no netdev under {phy}")))?;
        Ok(first)
    }
}

impl Drop for Hwsim {
    fn drop(&mut self) {
        // Best-effort — if the module is pinned by leftover test
        // artifacts we leak it; the next load will fail and the
        // next developer will investigate.
        let _ = Command::new("rmmod").arg("mac80211_hwsim").status();
    }
}

fn current_phys() -> std::io::Result<Vec<String>> {
    let dir = Path::new("/sys/class/ieee80211");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        out.push(entry.file_name().to_string_lossy().into_owned());
    }
    out.sort();
    Ok(out)
}

fn wait_until<F: FnMut() -> bool>(deadline: Duration, mut pred: F) -> Option<()> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if pred() {
            return Some(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

// ---------------------------------------------------------------------------
// hostapd (AP role)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct HostapdConfig {
    pub ifname: String,
    pub ssid: String,
    pub passphrase: String,
    pub channel: u32,
}

/// Spawn a hostapd instance against `cfg.ifname`. Drop kills it.
pub struct Hostapd {
    pub child: Child,
    pub config_path: PathBuf,
}

impl Hostapd {
    pub fn start(cfg: &HostapdConfig) -> std::io::Result<Self> {
        let dir = tempdir()?;
        let config_path = dir.join("hostapd.conf");
        std::fs::write(&config_path, render_hostapd(cfg))?;
        // `-B` would daemonize; we keep the child in-proc so Drop
        // can kill it cleanly.
        let child = Command::new("hostapd")
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // Give hostapd a beat to bring up the BSS before tests
        // probe — the manpage doesn't offer a "ready" signal.
        std::thread::sleep(Duration::from_millis(500));
        Ok(Self { child, config_path })
    }
}

impl Drop for Hostapd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(parent) = self.config_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

pub fn render_hostapd(cfg: &HostapdConfig) -> String {
    // Minimal WPA2-Personal (RSN) config: one SSID, CCMP only, no
    // PMF required so we also test the "capable" PMF policy. See
    // hostapd.conf(5) for the knobs.
    format!(
        "interface={ifname}\n\
         driver=nl80211\n\
         ssid={ssid}\n\
         hw_mode=g\n\
         channel={channel}\n\
         wpa=2\n\
         wpa_key_mgmt=WPA-PSK\n\
         wpa_pairwise=CCMP\n\
         rsn_pairwise=CCMP\n\
         wpa_passphrase={passphrase}\n",
        ifname = cfg.ifname,
        ssid = cfg.ssid,
        channel = cfg.channel,
        passphrase = cfg.passphrase,
    )
}

// ---------------------------------------------------------------------------
// wpa_supplicant (station role)
// ---------------------------------------------------------------------------

/// Spawn wpa_supplicant on `ifname` against the D-Bus config file
/// shipped by wpa_supplicant (the real backend attaches via
/// `fi.w1.wpa_supplicant1`, which requires D-Bus integration —
/// `-u` flag). Drop kills it.
pub struct WpaSupplicant {
    pub child: Child,
}

impl WpaSupplicant {
    /// Start wpa_supplicant bound to D-Bus. Assumes the system bus
    /// `fi.w1.wpa_supplicant1` is free (i.e. no distro-managed
    /// instance is running). Caller is responsible for ensuring
    /// that — typical CI runners have none.
    pub fn start(ifname: &str) -> std::io::Result<Self> {
        let child = Command::new("wpa_supplicant")
            .args(["-u", "-D", "nl80211", "-i", ifname])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::sleep(Duration::from_millis(500));
        Ok(Self { child })
    }
}

impl Drop for WpaSupplicant {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn ip_link_up(ifname: &str) -> std::io::Result<()> {
    let status = Command::new("ip")
        .args(["link", "set", "dev", ifname, "up"])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!("ip link up {ifname}")));
    }
    Ok(())
}

pub fn ip_addr_add(ifname: &str, addr: IpAddr, prefix: u8) -> std::io::Result<()> {
    let status = Command::new("ip")
        .args([
            "addr",
            "add",
            &format!("{addr}/{prefix}"),
            "dev",
            ifname,
        ])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!("ip addr add {ifname}")));
    }
    Ok(())
}

/// Convenience: assign 10.99.0.1/24 to the AP side so clients can
/// ping after association in smoke tests.
pub fn ap_side_address(ifname: &str) -> std::io::Result<()> {
    ip_addr_add(ifname, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 24)
}

fn tempdir() -> std::io::Result<PathBuf> {
    // We deliberately avoid adding `tempfile` as a dependency here
    // since the test crate already pulls it in; use a trivial
    // `mkstemp`-style dir under `/tmp` to keep this module
    // self-contained.
    let base = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = base.join(format!("nexus-hwsim-{nonce}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
