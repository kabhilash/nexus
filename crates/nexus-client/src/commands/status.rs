//! `nexusctl status`. DD-008 §5.1 / §11.
//!
//! Pulls the snapshot from `Manager.GetManagerStatus` and hands
//! off to the output dispatcher. No format-specific code lives
//! here by design — new output modes land in `output/` and every
//! command inherits them.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::ManagerOps;

pub async fn run(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let status = ops.get_manager_status().await?;
    render(&status, format, ctx, w).map_err(map_io_error)
}

fn map_io_error(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{InterfaceSummary, ManagerStatus};
    use async_trait::async_trait;

    struct StubOps;

    #[async_trait]
    impl ManagerOps for StubOps {
        async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError> {
            Ok(ManagerStatus {
                version: "0.1.0".into(),
                power_state: "active".into(),
                api_capabilities: vec![],
                interface_count: 3,
                wifi_profile_count: 1,
                ethernet_profile_count: 0,
                bluetooth_profile_count: 0,
                master_key_source: "file".into(),
            })
        }
        async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn json_status_round_trips_through_serde() {
        let mut buf = Vec::new();
        run(
            &StubOps,
            OutputFormat::Json,
            &RenderContext::default(),
            &mut buf,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["version"], "0.1.0");
        assert_eq!(v["power_state"], "active");
    }

    #[tokio::test]
    async fn human_status_renders_keys() {
        let mut buf = Vec::new();
        run(
            &StubOps,
            OutputFormat::Human,
            &RenderContext::default(),
            &mut buf,
        )
        .await
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Version:"));
        assert!(s.contains("Power state:"));
        assert!(s.contains("Master key:"));
    }
}
