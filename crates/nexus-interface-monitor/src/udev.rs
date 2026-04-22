//! udev-based discovery for Bluetooth adapters (`hci*` in the
//! `bluetooth` subsystem) and GNSS receivers (`tty*` in the `tty`
//! subsystem with the GNSS property heuristics from DD-001 §4.3).
//!
//! The initial-enumeration API uses `udev::Enumerator` (fast,
//! blocking; called once at cold boot). The hotplug side runs in a
//! dedicated OS thread and forwards translated [`UdevAction`]s
//! through a tokio mpsc — the `udev` crate's FFI pointers are
//! not `Send`, so we keep them on that thread rather than holding
//! them across an `.await`.

use std::ffi::OsStr;
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use udev::{Device, Enumerator, MonitorBuilder};

// ---------------------------------------------------------------------------
// Plain descriptions of what udev told us. The monitor task converts
// these into `InterfaceInfo` records.
// ---------------------------------------------------------------------------

/// One Bluetooth HCI adapter as seen by udev.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothAdapter {
    pub hci_name: String,
    pub hci_index: u32,
    pub bt_address: Option<[u8; 6]>,
    pub bluez_path: String,
}

/// One GNSS device as seen by udev.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GnssDevice {
    pub device_path: PathBuf,
    pub gpsd_device: String,
    pub vendor_model: Option<String>,
}

/// Hotplug event translated by the udev thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdevAction {
    BluetoothAdd(BluetoothAdapter),
    BluetoothRemove { hci_name: String, hci_index: u32 },
    GnssAdd(GnssDevice),
    GnssRemove { device_path: PathBuf },
}

// ---------------------------------------------------------------------------
// Initial enumeration.
// ---------------------------------------------------------------------------

/// Scan the `bluetooth` subsystem for HCI adapters. Devices without a
/// parseable `hciN` sysname are skipped.
pub fn enumerate_bluetooth() -> io::Result<Vec<BluetoothAdapter>> {
    let mut enumerator = Enumerator::new()?;
    enumerator.match_subsystem("bluetooth")?;

    let mut out = Vec::new();
    for device in enumerator.scan_devices()? {
        if let Some(adapter) = bluetooth_from_device(&device) {
            out.push(adapter);
        }
    }
    Ok(out)
}

/// Scan the `tty` subsystem for devices that look like GNSS receivers
/// per DD-001 §4.3's heuristics.
pub fn enumerate_gnss() -> io::Result<Vec<GnssDevice>> {
    let mut enumerator = Enumerator::new()?;
    enumerator.match_subsystem("tty")?;

    let mut out = Vec::new();
    for device in enumerator.scan_devices()? {
        if !looks_like_gnss(&device) {
            continue;
        }
        if let Some(dev) = gnss_from_device(&device) {
            out.push(dev);
        }
    }
    Ok(out)
}

fn bluetooth_from_device(device: &Device) -> Option<BluetoothAdapter> {
    let sysname = device.sysname().to_str()?;
    let rest = sysname.strip_prefix("hci")?;
    let hci_index: u32 = rest.parse().ok()?;
    let bt_address = device
        .attribute_value("address")
        .and_then(|s| s.to_str())
        .and_then(parse_colon_hex_mac);
    Some(BluetoothAdapter {
        hci_name: sysname.to_owned(),
        hci_index,
        bt_address,
        bluez_path: format!("/org/bluez/{sysname}"),
    })
}

fn looks_like_gnss(device: &Device) -> bool {
    if prop_eq(device, "NEXUS_GNSS", "1") {
        return true;
    }
    const KNOWN_USB_DRIVERS: &[&str] = &["cdc_acm", "pl2303", "cp210x", "ch341", "ftdi_sio"];
    if let Some(driver) = device
        .property_value("ID_USB_DRIVER")
        .and_then(|s| s.to_str())
    {
        if KNOWN_USB_DRIVERS.iter().any(|d| *d == driver) {
            return true;
        }
    }
    if let Some(model) = device.property_value("ID_MODEL").and_then(|s| s.to_str()) {
        let lower = model.to_ascii_lowercase();
        if lower.contains("gps")
            || lower.contains("gnss")
            || lower.contains("u-blox")
            || lower.contains("sirf")
        {
            return true;
        }
    }
    false
}

fn gnss_from_device(device: &Device) -> Option<GnssDevice> {
    let devnode = device.devnode()?.to_path_buf();
    let gpsd_device = devnode.to_string_lossy().into_owned();
    let vendor = device
        .property_value("ID_VENDOR")
        .and_then(|s| s.to_str())
        .map(|s| s.to_owned());
    let model = device
        .property_value("ID_MODEL")
        .and_then(|s| s.to_str())
        .map(|s| s.to_owned());
    let vendor_model = match (vendor, model) {
        (Some(v), Some(m)) => Some(format!("{v} {m}")),
        (Some(s), None) | (None, Some(s)) => Some(s),
        (None, None) => None,
    };
    Some(GnssDevice {
        device_path: devnode,
        gpsd_device,
        vendor_model,
    })
}

fn prop_eq(device: &Device, key: &str, value: &str) -> bool {
    device
        .property_value(key)
        .map(|v| v == OsStr::new(value))
        .unwrap_or(false)
}

/// Parse `"AA:BB:CC:DD:EE:FF"`. Accepts lower- or uppercase.
pub(crate) fn parse_colon_hex_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Hotplug monitor — driven on a dedicated OS thread.
// ---------------------------------------------------------------------------

/// Handle for consuming translated hotplug actions. Dropping the
/// handle signals the backing thread to exit.
pub struct UdevMonitorHandle {
    rx: mpsc::UnboundedReceiver<UdevAction>,
    shutdown: Arc<AtomicBool>,
}

impl UdevMonitorHandle {
    /// Spawn the udev monitor thread. Awaits the thread's startup so
    /// `MonitorBuilder` errors (no libudev on the host, missing
    /// perms) surface synchronously at setup time. `MonitorSocket`
    /// is not `Send`, so it has to be built *on* the worker thread
    /// rather than moved in.
    pub async fn spawn() -> io::Result<Self> {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);

        std::thread::Builder::new()
            .name("nexus-udev-monitor".into())
            .spawn(move || match build_monitor() {
                Ok(monitor) => {
                    let _ = startup_tx.send(Ok(()));
                    run_udev_thread(monitor, action_tx, shutdown_for_thread);
                }
                Err(e) => {
                    let _ = startup_tx.send(Err(e));
                }
            })?;

        match startup_rx.await {
            Ok(Ok(())) => Ok(Self {
                rx: action_rx,
                shutdown,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::other(
                "udev monitor thread aborted during startup",
            )),
        }
    }

    /// Wait for the next udev action. `None` means the monitor
    /// thread has exited.
    pub async fn next_action(&mut self) -> Option<UdevAction> {
        self.rx.recv().await
    }
}

fn build_monitor() -> io::Result<udev::MonitorSocket> {
    MonitorBuilder::new()?
        .match_subsystem("bluetooth")?
        .match_subsystem("tty")?
        .listen()
}

impl Drop for UdevMonitorHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn run_udev_thread(
    monitor: udev::MonitorSocket,
    tx: mpsc::UnboundedSender<UdevAction>,
    shutdown: Arc<AtomicBool>,
) {
    let fd = monitor.as_raw_fd();
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    while !shutdown.load(Ordering::Acquire) {
        // 250 ms poll budget so Drop can wake us up in a bounded
        // window without busy-looping.
        // SAFETY: `poll(2)` takes a pointer to one `pollfd`, which we
        // own on the stack, for one slot. The timeout is in
        // milliseconds.
        let rc = unsafe { libc::poll(&mut pollfd as *mut libc::pollfd, 1, 250) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!(error = %err, "udev poll failed; exiting monitor thread");
            return;
        }
        if rc == 0 || pollfd.revents & libc::POLLIN == 0 {
            continue;
        }

        for event in monitor.iter() {
            let action = match translate_event(&event) {
                Some(a) => a,
                None => continue,
            };
            if tx.send(action).is_err() {
                // Main loop dropped the receiver.
                return;
            }
        }
    }
}

fn translate_event(event: &udev::Event) -> Option<UdevAction> {
    let device = event.device();
    let subsystem = device.subsystem()?.to_str()?.to_owned();
    let action = event.event_type();
    match (subsystem.as_str(), action) {
        ("bluetooth", udev::EventType::Add) => {
            let adapter = bluetooth_from_device(&device)?;
            Some(UdevAction::BluetoothAdd(adapter))
        }
        ("bluetooth", udev::EventType::Remove) => {
            let adapter = bluetooth_from_device(&device)?;
            Some(UdevAction::BluetoothRemove {
                hci_name: adapter.hci_name,
                hci_index: adapter.hci_index,
            })
        }
        ("tty", udev::EventType::Add) => {
            if !looks_like_gnss(&device) {
                return None;
            }
            let dev = gnss_from_device(&device)?;
            Some(UdevAction::GnssAdd(dev))
        }
        ("tty", udev::EventType::Remove) => {
            let devnode = device.devnode()?.to_path_buf();
            Some(UdevAction::GnssRemove {
                device_path: devnode,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_mac_accepts_canonical_form() {
        assert_eq!(
            parse_colon_hex_mac("AA:BB:CC:DD:EE:FF"),
            Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
        );
    }

    #[test]
    fn parse_hex_mac_accepts_lowercase() {
        assert_eq!(
            parse_colon_hex_mac("aa:bb:cc:dd:ee:ff"),
            Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
        );
    }

    #[test]
    fn parse_hex_mac_rejects_malformed_input() {
        assert!(parse_colon_hex_mac("AA:BB:CC:DD:EE").is_none());
        assert!(parse_colon_hex_mac("ZZ:BB:CC:DD:EE:FF").is_none());
        assert!(parse_colon_hex_mac("AA-BB-CC-DD-EE-FF").is_none());
    }
}
