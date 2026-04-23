//! `nexusctl iface list`. DD-008 §5.1 / §11.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, human, json};
use crate::proxy::ManagerOps;

pub async fn list(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let rows = ops.list_interfaces().await?;
    match format {
        OutputFormat::Human => human::render_iface_list(&rows, w).map_err(io_to_err),
        OutputFormat::Json => json::write(&rows, w).map_err(io_to_err),
    }
}

fn io_to_err(e: std::io::Error) -> NexusctlError {
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
                state: "dormant".into(),
                mac: Some("aa:bb:cc:dd:ee:03".into()),
                carrier: false,
            },
            InterfaceSummary {
                iface: "/dev/gps0".into(),
                kind: "gnss".into(),
                state: "up".into(),
                mac: None,
                carrier: false,
            },
        ]
    }

    #[tokio::test]
    async fn human_list_renders_table_with_each_row() {
        let ops = StubOps { rows: fixture() };
        let mut buf = Vec::new();
        list(&ops, OutputFormat::Human, &mut buf).await.unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("IFACE"));
        assert!(s.contains("KIND"));
        assert!(s.contains("STATE"));
        assert!(s.contains("MAC"));
        assert!(s.contains("eth0"));
        assert!(s.contains("ethernet"));
        assert!(s.contains("aa:bb:cc:dd:ee:01"));
        assert!(s.contains("wlan0"));
        // GNSS row has no MAC — em-dash placeholder.
        assert!(s.contains("/dev/gps0"));
        assert!(s.contains("—"));
    }

    #[tokio::test]
    async fn human_list_empty_says_no_interfaces() {
        let ops = StubOps { rows: vec![] };
        let mut buf = Vec::new();
        list(&ops, OutputFormat::Human, &mut buf).await.unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("no interfaces"));
    }

    #[tokio::test]
    async fn json_list_emits_array_of_flat_objects() {
        let ops = StubOps { rows: fixture() };
        let mut buf = Vec::new();
        list(&ops, OutputFormat::Json, &mut buf).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["iface"], "eth0");
        assert_eq!(arr[0]["kind"], "ethernet");
        assert_eq!(arr[0]["state"], "up");
        assert_eq!(arr[0]["mac"], "aa:bb:cc:dd:ee:01");
        assert_eq!(arr[0]["carrier"], true);
        // GNSS row's MAC is null — DD-008 §5.3 "Missing /
        // unavailable values are null".
        assert!(arr[2]["mac"].is_null());
    }
}
