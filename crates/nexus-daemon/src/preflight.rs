//! Startup self-test. Runs after config load and before any
//! subsystem spawns.
//!
//! Walks the effective config and probes the environment for each
//! thing the enabled subsystems depend on — profile store root being
//! writable, master key file being readable, external daemons being
//! installed on `$PATH`. Every finding carries a concrete action the
//! operator can take to fix it, so a fresh install that's missing,
//! say, `wpa_supplicant` surfaces as:
//!
//! ```text
//! nexusd: [warn] wifi: wpa_supplicant binary not found on PATH
//!        -> install it: apt install wpasupplicant
//!                       (or: dnf install wpa_supplicant)
//! ```
//!
//! Severity policy:
//! - `Fatal`: daemon cannot usefully start. Preflight writes the
//!   line to stderr and returns an error from `main`, exit 1.
//! - `Warn`: the subsystem will fail or degrade once it tries to
//!   talk to its external daemon. Preflight still prints the action
//!   item — so an operator debugging "why doesn't Wi-Fi work" sees
//!   it upfront — but the daemon proceeds, because the subsystem's
//!   own reconnect loop is the long-term contract (DD-001 §3.6
//!   "NameOwnerChanged-driven reconnect").

use std::io::Write;
use std::path::Path;

use crate::config::Config;

/// Severity of a preflight finding. See the module docstring for the
/// policy distinguishing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Fatal,
    Warn,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Fatal => "fatal",
            Severity::Warn => "warn",
        }
    }
}

/// One thing preflight found that needs attention. `subject` names
/// the subsystem/facet (e.g. `"wifi"`, `"profile_store"`), `issue`
/// is the one-line observation, and `action` is the concrete fix.
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub subject: &'static str,
    pub issue: String,
    pub action: String,
}

impl Finding {
    fn fatal(subject: &'static str, issue: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            severity: Severity::Fatal,
            subject,
            issue: issue.into(),
            action: action.into(),
        }
    }

    fn warn(subject: &'static str, issue: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warn,
            subject,
            issue: issue.into(),
            action: action.into(),
        }
    }
}

/// Run every preflight probe against `config` and return the
/// findings in declaration order. An empty vec means the environment
/// is healthy as far as we can tell.
pub fn run(config: &Config) -> Vec<Finding> {
    run_with_path(config, std::env::var_os("PATH").as_deref())
}

/// Same as [`run`] but takes an explicit `$PATH` lookup string.
/// Exposed for tests that need to simulate a missing binary without
/// mutating the process-wide environment (which would race under
/// parallel test threads).
pub fn run_with_path(config: &Config, path_var: Option<&std::ffi::OsStr>) -> Vec<Finding> {
    run_with(config, path_var, &current_user_groups())
}

/// Fully-parametrised preflight. Both the `$PATH` string and the
/// calling user's group list are injected so tests can exercise
/// every branch deterministically.
pub fn run_with(
    config: &Config,
    path_var: Option<&std::ffi::OsStr>,
    user_groups: &[String],
) -> Vec<Finding> {
    let mut out = Vec::new();
    check_profile_store_root(&config.profile_store.root, &mut out);
    check_master_key_source(&config.profile_store, &mut out);
    check_external_daemons(config, path_var, &mut out);
    check_group_memberships(config, user_groups, &mut out);
    out
}

/// Render `findings` to `out` in a stable, grep-friendly format and
/// return `true` when any finding is [`Severity::Fatal`] — callers
/// use that to decide whether to abort the daemon.
pub fn report(findings: &[Finding], out: &mut dyn Write) -> std::io::Result<bool> {
    let mut any_fatal = false;
    for f in findings {
        any_fatal |= f.severity == Severity::Fatal;
        writeln!(
            out,
            "nexusd: [{sev}] {subj}: {issue}",
            sev = f.severity.label(),
            subj = f.subject,
            issue = f.issue,
        )?;
        writeln!(out, "       -> {}", f.action)?;
    }
    Ok(any_fatal)
}

// ---- individual checks ----------------------------------------------------

fn check_profile_store_root(root: &Path, out: &mut Vec<Finding>) {
    // Missing directory is OK — `ProfileFileStore::open` creates it.
    // What we can't recover from is an unwritable existing path.
    if root.exists() && !root.is_dir() {
        out.push(Finding::fatal(
            "profile_store",
            format!("{} exists but is not a directory", root.display()),
            format!(
                "remove the file or point profile_store.root elsewhere: \
                 rm {p}    or    edit nexus.toml [profile_store] root = ...",
                p = root.display()
            ),
        ));
        return;
    }
    // Try to create it, then write a probe file. Cleans up after
    // itself so we don't leave cruft in the profile root. Anything
    // that fails at this stage will also fail when the real store
    // opens a few lines later — surfacing it here lets us bundle the
    // fix with the other action items.
    if let Err(e) = std::fs::create_dir_all(root) {
        out.push(Finding::fatal(
            "profile_store",
            format!("cannot create {}: {e}", root.display()),
            format!(
                "create the directory and make it owned by nexus: \
                 sudo install -d -m 0700 -o nexus -g nexus {p}",
                p = root.display()
            ),
        ));
        return;
    }
    let probe = root.join(".preflight-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(e) => out.push(Finding::fatal(
            "profile_store",
            format!("{} is not writable: {e}", root.display()),
            format!(
                "grant the nexus user write access: \
                 sudo chown -R nexus:nexus {p} && sudo chmod 0700 {p}",
                p = root.display()
            ),
        )),
    }
}

fn check_master_key_source(section: &crate::ProfileStoreSection, out: &mut Vec<Finding>) {
    match section.key_source.as_str() {
        "file" => {
            let key = section.root.join("keys").join("master.key");
            if key.exists() {
                // If the file is there but we can't read it, the
                // store will panic on open; fatal preflight finding
                // with a permissions fix.
                if let Err(e) = std::fs::File::open(&key) {
                    out.push(Finding::fatal(
                        "profile_store",
                        format!("cannot open {}: {e}", key.display()),
                        format!(
                            "fix ownership so the nexus user can read the key: \
                             sudo chown nexus:nexus {p} && sudo chmod 0400 {p}",
                            p = key.display()
                        ),
                    ));
                }
            }
            // Missing key is not a finding — FileKeySource generates
            // one the first time the store opens. Config load
            // already rejects insecure fallbacks.
        }
        "in_memory" => {
            // Config::load already validates that in_memory_seed is
            // present and 64 hex chars. Nothing more to check here.
        }
        other => {
            out.push(Finding::fatal(
                "profile_store",
                format!("unknown key_source {other:?}"),
                "set profile_store.key_source to \"file\" or \"in_memory\" in nexus.toml",
            ));
        }
    }
}

fn check_external_daemons(
    config: &Config,
    path_var: Option<&std::ffi::OsStr>,
    out: &mut Vec<Finding>,
) {
    // Wi-Fi: only the real `wpa_supplicant` backend needs the binary.
    // The `mock` backend is used by tests and the `iwd` backend is
    // recognised but not wired up yet — skip the PATH check for both.
    if config.wifi.enabled
        && config.wifi.backend == "wpa_supplicant"
        && !binary_in("wpa_supplicant", path_var)
    {
        out.push(Finding::warn(
            "wifi",
            "wpa_supplicant binary not found on PATH",
            "install it (apt install wpasupplicant, \
             or dnf install wpa_supplicant) and enable its service: \
             systemctl enable --now wpa_supplicant",
        ));
    }

    // Ethernet 802.1X reuses wpa_supplicant when configured. `none`
    // means no 802.1X, `ead` is a planned alternative — both skip the
    // check.
    if config.ethernet.enabled
        && config.ethernet.auth_backend == "wpa_supplicant"
        && !binary_in("wpa_supplicant", path_var)
    {
        out.push(Finding::warn(
            "ethernet",
            "wpa_supplicant binary not found on PATH (needed for 802.1X)",
            "install it (apt install wpasupplicant) or set ethernet.auth_backend = \"none\" \
             if 802.1X is not required",
        ));
    }

    // Bluetooth: the real backend talks to BlueZ over D-Bus. BlueZ
    // ships `bluetoothd` — its presence on PATH is a reasonable proxy
    // for "the platform has BlueZ installed." The subsystem's own
    // reconnect loop handles the "installed but not running" case.
    if config.bluetooth.enabled && !config.bluetooth.mock && !binary_in("bluetoothd", path_var) {
        out.push(Finding::warn(
            "bluetooth",
            "bluetoothd not found on PATH — BlueZ is probably not installed",
            "install BlueZ (apt install bluez, or dnf install bluez) and enable its service: \
             systemctl enable --now bluetooth",
        ));
    }

    // GNSS talks to gpsd over a TCP socket; the daemon binary being
    // absent is a near-certain sign the endpoint won't answer.
    if config.gnss.enabled && !config.gnss.mock && !binary_in("gpsd", path_var) {
        out.push(Finding::warn(
            "gnss",
            "gpsd binary not found on PATH",
            "install it (apt install gpsd, or dnf install gpsd) and point the service at \
             your GNSS device — see gpsd(8) and gpsd.socket(8)",
        ));
    }
}

/// Check that the running user is in any privileged groups enabled
/// subsystems depend on. The real symptom of a missing group is a
/// cryptic `AccessDenied` from the affected daemon's bus policy; the
/// warning here turns that into a one-line action item.
fn check_group_memberships(config: &Config, user_groups: &[String], out: &mut Vec<Finding>) {
    // Wi-Fi: wpa_supplicant's default system-bus policy
    // (/usr/share/dbus-1/system.d/wpa_supplicant.conf) restricts
    // CreateInterface / RemoveInterface to root + members of
    // `netdev`. Missing membership produces:
    //   AccessDenied: Rejected send message, 2 matched rules; ...
    //     member="CreateInterface"
    if config.wifi.enabled
        && config.wifi.backend == "wpa_supplicant"
        && !user_groups.iter().any(|g| g == "netdev")
    {
        out.push(Finding::warn(
            "wifi",
            "the daemon user is not in the `netdev` group — \
             wpa_supplicant will reject CreateInterface with AccessDenied",
            "add the user and restart: \
             sudo usermod -aG netdev nexus && sudo systemctl restart nexus",
        ));
    }

    // Bluetooth: BlueZ's system-bus policy on most distros grants
    // privileged access to members of `bluetooth`. Without it, the
    // adapter state-change methods fail with AccessDenied once the
    // backend gets past introspection.
    if config.bluetooth.enabled
        && !config.bluetooth.mock
        && !user_groups.iter().any(|g| g == "bluetooth")
    {
        out.push(Finding::warn(
            "bluetooth",
            "the daemon user is not in the `bluetooth` group — \
             BlueZ may reject privileged methods with AccessDenied",
            "add the user and restart: \
             sudo usermod -aG bluetooth nexus && sudo systemctl restart nexus",
        ));
    }
}

/// Current process's supplementary group *names*. Invokes
/// `id -Gn` rather than calling `getgroups(2)` + `getgrgid(3)` so
/// the dependency surface stays small (no `libc` in production
/// deps) — the fork/exec cost is one-shot at daemon startup. An
/// empty vec on failure is treated as "no groups," producing the
/// same warnings as a genuinely unprivileged user.
fn current_user_groups() -> Vec<String> {
    std::process::Command::new("id")
        .arg("-Gn")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .map(|s| s.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// `which`-equivalent with an explicit `$PATH` string. Takes the
/// lookup string as a parameter rather than reading the process env
/// so tests can drive it without mutating the process-wide PATH
/// (which would race across parallel test threads). Callers passing
/// `std::env::var_os("PATH").as_deref()` get the usual behaviour.
fn binary_in(name: &str, path_var: Option<&std::ffi::OsStr>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn base_config(root: PathBuf) -> Config {
        let mut cfg = Config::default();
        cfg.profile_store.root = root;
        cfg.profile_store.key_source = "in_memory".into();
        cfg.profile_store.in_memory_seed = Some("0".repeat(64));
        // Keep every subsystem off so individual tests opt in.
        cfg.interface_monitor.enabled = false;
        cfg.ethernet.enabled = false;
        cfg.wifi.enabled = false;
        cfg.bluetooth.enabled = false;
        cfg.gnss.enabled = false;
        cfg.dbus.enabled = false;
        cfg
    }

    #[test]
    fn clean_config_yields_no_findings() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = base_config(tmp.path().join("store"));
        let findings = run(&cfg);
        assert!(findings.is_empty(), "findings: {findings:?}");
    }

    #[test]
    fn profile_store_root_that_is_a_file_is_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store-file");
        std::fs::write(&path, b"junk").unwrap();
        let cfg = base_config(path);
        let findings = run(&cfg);
        assert_eq!(findings.len(), 1, "got: {findings:?}");
        assert_eq!(findings[0].severity, Severity::Fatal);
        assert_eq!(findings[0].subject, "profile_store");
        assert!(findings[0].action.contains("rm"));
    }

    /// An OsStr that points at a definitely-empty search path. Used
    /// to simulate "binary not installed" without racing on the
    /// real PATH.
    fn empty_path() -> std::ffi::OsString {
        std::ffi::OsString::from("/nonexistent")
    }

    /// Groups list that covers every privileged membership the
    /// preflight group-checker looks for. Tests pre-seed this so
    /// the group check never fires unless a test explicitly opts in.
    fn all_groups() -> Vec<String> {
        vec!["netdev".into(), "bluetooth".into()]
    }

    #[test]
    fn wifi_enabled_with_wpa_supplicant_backend_needs_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path().join("store"));
        cfg.wifi.enabled = true;
        cfg.wifi.backend = "wpa_supplicant".into();
        let findings = run_with(&cfg, Some(&empty_path()), &all_groups());
        assert_eq!(findings.len(), 1, "got: {findings:?}");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert_eq!(findings[0].subject, "wifi");
        assert!(findings[0].action.contains("wpasupplicant"));
    }

    #[test]
    fn mock_backends_skip_external_daemon_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path().join("store"));
        cfg.wifi.enabled = true;
        cfg.wifi.backend = "mock".into();
        cfg.bluetooth.enabled = true;
        cfg.bluetooth.mock = true;
        cfg.gnss.enabled = true;
        cfg.gnss.mock = true;
        let findings = run_with(&cfg, Some(&empty_path()), &all_groups());
        assert!(findings.is_empty(), "mock backends triggered: {findings:?}");
    }

    #[test]
    fn ethernet_802_1x_with_wpa_supplicant_auth_backend_triggers_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path().join("store"));
        cfg.ethernet.enabled = true;
        cfg.ethernet.auth_backend = "wpa_supplicant".into();
        let findings = run_with(&cfg, Some(&empty_path()), &all_groups());
        assert_eq!(findings.len(), 1, "got: {findings:?}");
        assert_eq!(findings[0].subject, "ethernet");
        assert!(findings[0].issue.contains("802.1X"));
    }

    #[test]
    fn bluetooth_and_gnss_warn_when_daemons_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path().join("store"));
        cfg.bluetooth.enabled = true;
        cfg.bluetooth.mock = false;
        cfg.gnss.enabled = true;
        cfg.gnss.mock = false;
        let findings = run_with(&cfg, Some(&empty_path()), &all_groups());
        let subjects: Vec<_> = findings.iter().map(|f| f.subject).collect();
        assert!(subjects.contains(&"bluetooth"), "got: {subjects:?}");
        assert!(subjects.contains(&"gnss"), "got: {subjects:?}");
    }

    #[test]
    fn wifi_user_not_in_netdev_group_triggers_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path().join("store"));
        cfg.wifi.enabled = true;
        cfg.wifi.backend = "wpa_supplicant".into();
        // Pretend the binary exists (path_var = None → the binary
        // check short-circuits with "PATH missing" → false, which
        // is the same as the binary being absent). Use an explicit
        // PATH that contains this test's own workspace root so the
        // binary check passes vacuously... actually easier: build a
        // dir that contains a stub `wpa_supplicant` file.
        let bindir = tmp.path().join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let stub = bindir.join("wpa_supplicant");
        std::fs::write(&stub, b"").unwrap();
        let path_var = std::ffi::OsString::from(bindir.as_os_str());

        // No group memberships — the netdev check must fire.
        let findings = run_with(&cfg, Some(&path_var), &[]);
        assert_eq!(findings.len(), 1, "got: {findings:?}");
        assert_eq!(findings[0].subject, "wifi");
        assert!(findings[0].issue.contains("netdev"));
        assert!(findings[0].action.contains("usermod -aG netdev"));
    }

    #[test]
    fn bluetooth_user_not_in_bluetooth_group_triggers_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path().join("store"));
        cfg.bluetooth.enabled = true;
        cfg.bluetooth.mock = false;
        let bindir = tmp.path().join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        std::fs::write(bindir.join("bluetoothd"), b"").unwrap();
        let path_var = std::ffi::OsString::from(bindir.as_os_str());

        let findings = run_with(&cfg, Some(&path_var), &[]);
        assert_eq!(findings.len(), 1, "got: {findings:?}");
        assert_eq!(findings[0].subject, "bluetooth");
        assert!(findings[0].issue.contains("bluetooth"));
        assert!(findings[0].action.contains("usermod -aG bluetooth"));
    }

    #[test]
    fn group_check_silent_when_membership_present() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path().join("store"));
        cfg.wifi.enabled = true;
        cfg.wifi.backend = "wpa_supplicant".into();
        cfg.bluetooth.enabled = true;
        cfg.bluetooth.mock = false;
        let bindir = tmp.path().join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        std::fs::write(bindir.join("wpa_supplicant"), b"").unwrap();
        std::fs::write(bindir.join("bluetoothd"), b"").unwrap();
        let path_var = std::ffi::OsString::from(bindir.as_os_str());

        let findings = run_with(&cfg, Some(&path_var), &all_groups());
        assert!(findings.is_empty(), "findings: {findings:?}");
    }

    #[test]
    fn report_format_is_grep_friendly_and_flags_fatals() {
        let findings = vec![
            Finding::warn("wifi", "binary missing", "install it"),
            Finding::fatal("profile_store", "path is read-only", "chmod it"),
        ];
        let mut buf = Cursor::new(Vec::new());
        let any_fatal = report(&findings, &mut buf).unwrap();
        assert!(any_fatal);
        let s = String::from_utf8(buf.into_inner()).unwrap();
        // One line per finding + one "action" follow-up.
        assert!(s.contains("nexusd: [warn] wifi: binary missing"));
        assert!(s.contains("       -> install it"));
        assert!(s.contains("nexusd: [fatal] profile_store: path is read-only"));
        assert!(s.contains("       -> chmod it"));
    }

    #[test]
    fn report_returns_false_when_no_fatals() {
        let findings = vec![Finding::warn("x", "y", "z")];
        let mut buf = Cursor::new(Vec::new());
        let any_fatal = report(&findings, &mut buf).unwrap();
        assert!(!any_fatal);
    }
}
