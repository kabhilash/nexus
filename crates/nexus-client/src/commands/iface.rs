//! `nexusctl iface list`. DD-008 §5.1 / §11.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::ManagerOps;

pub async fn list(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let rows = ops.list_interfaces().await?;
    render(&rows, format, ctx, w).map_err(map_io_error)
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

    struct StubOps {
        rows: Vec<InterfaceSummary>,
    }

    #[async_trait]
    impl ManagerOps for StubOps {
        async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError> {
            unreachable!("status not used by iface tests")
        }
        async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError> {
            Ok(self.rows.clone())
        }
    }

    fn fixture() -> Vec<InterfaceSummary> {
        vec![
            InterfaceSummary {
                iface: "eth0".into(),
                kind: "ethernet".into(),
                state: "up".into(),
                mac: Some("aa:bb:cc:dd:ee:01".into()),
                carrier: true,
            },
            InterfaceSummary {
                iface: "wlan0".into(),
                kind: "wifi".into(),
                state: "connected".into(),
                mac: Some("aa:bb:cc:dd:ee:03".into()),
                carrier: true,
            },
        ]
    }

    #[tokio::test]
    async fn human_iface_list_has_prefix_column() {
        let ops = StubOps { rows: fixture() };
        let mut buf = Vec::new();
        list(
            &ops,
            OutputFormat::Human,
            &RenderContext::default(),
            &mut buf,
        )
        .await
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        // `*O` is the state prefix for an up ethernet with carrier.
        assert!(s.contains("*O"), "got {s}");
        assert!(s.contains("IFACE"));
        assert!(s.contains("eth0"));
        assert!(s.contains("wlan0"));
    }

    #[tokio::test]
    async fn json_iface_list_emits_array_of_flat_objects() {
        let ops = StubOps { rows: fixture() };
        let mut buf = Vec::new();
        list(
            &ops,
            OutputFormat::Json,
            &RenderContext::default(),
            &mut buf,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["iface"], "eth0");
        assert_eq!(arr[1]["kind"], "wifi");
    }
}
