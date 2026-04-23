//! `nexusctl status` — daemon overview. DD-008 §5.1 / §11.
//!
//! Calls `Manager.GetManagerStatus`, renders it in the requested
//! format. BlueZ / gpsd availability is spec'd as part of the
//! human-mode output but the daemon doesn't yet surface those
//! fields in `GetManagerStatus`; Phase 2 of DD-008 widens the dict
//! and this handler grows two more rows.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, human, json};
use crate::proxy::ManagerOps;

pub async fn run(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let status = ops.get_manager_status().await?;
    match format {
        OutputFormat::Human => human::render_status(&status, w).map_err(io_to_err),
        OutputFormat::Json => json::write(&status, w).map_err(io_to_err),
    }
}

fn io_to_err(e: std::io::Error) -> NexusctlError {
    // EPIPE while piping to `head` etc. is the most common stdout
    // failure path. Keep behaviour consistent with `ls` — silently
    // succeed; let the OS clean up the broken pipe at exit.
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other(String::new());
    }
    NexusctlError::Other(format!("write failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{InterfaceSummary, ManagerStatus};
    use async_trait::async_trait;

    struct StubOps {
        status: ManagerStatus,
    }

    #[async_trait]
    impl ManagerOps for StubOps {
        async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError> {
            Ok(self.status.clone())
        }
        async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError> {
            Ok(vec![])
        }
    }

    fn fixture() -> ManagerStatus {
        ManagerStatus {
            version: "0.1.0".into(),
            power_state: "active".into(),
            api_capabilities: vec![],
            interface_count: 3,
            wifi_profile_count: 1,
            ethernet_profile_count: 0,
            bluetooth_profile_count: 0,
            master_key_source: "file".into(),
        }
    }

    #[tokio::test]
    async fn human_status_includes_version_and_power_state() {
        let ops = StubOps { status: fixture() };
        let mut buf = Vec::new();
        run(&ops, OutputFormat::Human, &mut buf).await.unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Version:"));
        assert!(s.contains("0.1.0"));
        assert!(s.contains("Power state:"));
        assert!(s.contains("active"));
        assert!(s.contains("Interfaces:"));
        assert!(s.contains("3"));
        assert!(s.contains("Master key:"));
    }

    #[tokio::test]
    async fn json_status_round_trips_through_serde() {
        let ops = StubOps { status: fixture() };
        let mut buf = Vec::new();
        run(&ops, OutputFormat::Json, &mut buf).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["version"], "0.1.0");
        assert_eq!(v["power_state"], "active");
        assert_eq!(v["interface_count"], 3);
        assert_eq!(v["wifi_profile_count"], 1);
        assert_eq!(v["master_key_source"], "file");
    }
}
