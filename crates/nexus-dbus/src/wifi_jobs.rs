//! Per-interface tracking of in-flight Wi-Fi `Connect` / `Disconnect`
//! jobs.
//!
//! Each `Wifi.Connect` and `Wifi.Disconnect` call returns a ULID
//! `job_id` immediately and resolves later via a typed
//! `Wifi.ConnectComplete` / `Wifi.DisconnectComplete` signal (DD-006
//! §6.3 / §9). The completion edge for a `Connect` is emitted from
//! the service event loop the next time the interface reaches
//! `Connected` (success) or `Disconnected{reason}` (failure); the
//! tracker stores the pending `(ifname, job_id)` so the loop can
//! correlate the state transition with the job.
//!
//! Disconnect tracking is only used to emit
//! `Wifi.DisconnectComplete` from the spawned task that drives the
//! backend call — there's no event-loop correlation needed.

use std::collections::HashMap;
use std::sync::Mutex;

/// Tracker for outstanding `Wifi.Connect` / `Wifi.Disconnect` jobs.
#[derive(Default)]
pub struct WifiJobs {
    /// `ifname -> job_id` for an in-flight Connect.
    connect: Mutex<HashMap<String, String>>,
    /// `ifname -> job_id` for an in-flight Disconnect.
    disconnect: Mutex<HashMap<String, String>>,
}

impl WifiJobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `(ifname, job_id)` as the active Connect job.
    /// Replaces any prior entry — only one Connect job per
    /// interface is tracked at a time.
    pub fn register_connect(&self, ifname: &str, job_id: &str) {
        self.connect
            .lock()
            .unwrap()
            .insert(ifname.to_owned(), job_id.to_owned());
    }

    /// Remove and return the active Connect job for `ifname`, if any.
    /// Used both when the next state transition resolves the job and
    /// when an explicit Disconnect cancels it.
    pub fn take_connect(&self, ifname: &str) -> Option<String> {
        self.connect.lock().unwrap().remove(ifname)
    }

    pub fn register_disconnect(&self, ifname: &str, job_id: &str) {
        self.disconnect
            .lock()
            .unwrap()
            .insert(ifname.to_owned(), job_id.to_owned());
    }

    pub fn take_disconnect(&self, ifname: &str) -> Option<String> {
        self.disconnect.lock().unwrap().remove(ifname)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_take_returns_job_id() {
        let j = WifiJobs::new();
        j.register_connect("wlan0", "JOB1");
        assert_eq!(j.take_connect("wlan0").as_deref(), Some("JOB1"));
        assert!(j.take_connect("wlan0").is_none());
    }

    #[test]
    fn register_replaces_prior_connect_job() {
        let j = WifiJobs::new();
        j.register_connect("wlan0", "JOB1");
        j.register_connect("wlan0", "JOB2");
        assert_eq!(j.take_connect("wlan0").as_deref(), Some("JOB2"));
    }

    #[test]
    fn connect_and_disconnect_are_independent() {
        let j = WifiJobs::new();
        j.register_connect("wlan0", "C1");
        j.register_disconnect("wlan0", "D1");
        assert_eq!(j.take_disconnect("wlan0").as_deref(), Some("D1"));
        // Connect job still present.
        assert_eq!(j.take_connect("wlan0").as_deref(), Some("C1"));
    }

    #[test]
    fn jobs_are_per_ifname() {
        let j = WifiJobs::new();
        j.register_connect("wlan0", "A");
        j.register_connect("wlan1", "B");
        assert_eq!(j.take_connect("wlan1").as_deref(), Some("B"));
        assert_eq!(j.take_connect("wlan0").as_deref(), Some("A"));
    }
}
