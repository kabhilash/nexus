//! Snapshot coverage of every format × fixture combination.
//! DD-008 §11.1 "Output format" test plank.
//!
//! Uses insta inline snapshots so the expected text lives next to
//! the assertion and `cargo insta review` handles drift.

use std::sync::Arc;

use async_trait::async_trait;
use insta::assert_snapshot;
use nexus_client::commands;
use nexus_client::errors::NexusctlError;
use nexus_client::output::{OutputFormat, RenderContext};
use nexus_client::proxy::{InterfaceSummary, ManagerOps, ManagerStatus};

/// Deterministic manager stub — the same output data across every
/// snapshot so the expected text is meaningful.
struct Fixture {
    status: ManagerStatus,
    rows: Vec<InterfaceSummary>,
}

#[async_trait]
impl ManagerOps for Fixture {
    async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError> {
        Ok(self.status.clone())
    }
    async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError> {
        Ok(self.rows.clone())
    }
}

fn fixture() -> Arc<Fixture> {
    Arc::new(Fixture {
        status: ManagerStatus {
            version: "0.1.0".into(),
            power_state: "active".into(),
            api_capabilities: vec!["events".into(), "properties".into()],
            interface_count: 4,
            ethernet_count: 1,
            wifi_count: 1,
            bluetooth_count: 1,
            gnss_count: 1,
            wifi_profile_count: 2,
            ethernet_profile_count: 1,
            bluetooth_profile_count: 0,
            master_key_source: "file".into(),
            bluez_available: true,
            gpsd_available: true,
            internet_connectivity: "internetOnline".into(),
        },
        rows: vec![
            InterfaceSummary {
                iface: "eth0".into(),
                kind: "ethernet".into(),
                state: "up".into(),
                mac: Some("aa:bb:cc:dd:ee:01".into()),
                carrier: true,
                managed_profile: None,
            },
            InterfaceSummary {
                iface: "wlan0".into(),
                kind: "wifi".into(),
                state: "connected".into(),
                mac: Some("aa:bb:cc:dd:ee:03".into()),
                carrier: true,
                managed_profile: Some("/fi/nexus1/profile/wifi/X".into()),
            },
            InterfaceSummary {
                iface: "hci0".into(),
                kind: "bluetooth".into(),
                state: "powered".into(),
                mac: Some("dd:ee:ff:00:11:22".into()),
                carrier: false,
                managed_profile: None,
            },
            InterfaceSummary {
                iface: "/dev/gps0".into(),
                kind: "gnss".into(),
                state: "tracking".into(),
                mac: None,
                carrier: false,
                managed_profile: None,
            },
        ],
    })
}

async fn render_iface_list(format: OutputFormat, ctx: &RenderContext) -> String {
    let ops = fixture();
    let mut buf = Vec::new();
    commands::iface::list(ops.as_ref(), None, format, ctx, &mut buf)
        .await
        .expect("render");
    String::from_utf8(buf).unwrap()
}

async fn render_status(format: OutputFormat, ctx: &RenderContext) -> String {
    let ops = fixture();
    let mut buf = Vec::new();
    commands::status::run(ops.as_ref(), format, ctx, &mut buf)
        .await
        .expect("render");
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn snapshot_iface_list_human() {
    let s = render_iface_list(OutputFormat::Human, &RenderContext::default()).await;
    assert_snapshot!(s, @r"
          IFACE      KIND       STATE      MAC
     *O   eth0       ethernet   up         aa:bb:cc:dd:ee:01
     *AO  wlan0      wifi       connected  aa:bb:cc:dd:ee:03
     *R   hci0       bluetooth  powered    dd:ee:ff:00:11:22
     *F   /dev/gps0  gnss       tracking   —
    ");
}

#[tokio::test]
async fn snapshot_iface_list_terse() {
    let s = render_iface_list(OutputFormat::Terse, &RenderContext::default()).await;
    assert_snapshot!(s, @r"
    eth0:ethernet:up:aa\:bb\:cc\:dd\:ee\:01:true
    wlan0:wifi:connected:aa\:bb\:cc\:dd\:ee\:03:true
    hci0:bluetooth:powered:dd\:ee\:ff\:00\:11\:22:false
    /dev/gps0:gnss:tracking::false
    ");
}

#[tokio::test]
async fn snapshot_iface_list_terse_with_fields() {
    let ctx = RenderContext {
        fields: Some(vec!["iface".into(), "state".into()]),
        ..RenderContext::default()
    };
    let s = render_iface_list(OutputFormat::Terse, &ctx).await;
    assert_snapshot!(s, @r"
    eth0:up
    wlan0:connected
    hci0:powered
    /dev/gps0:tracking
    ");
}

#[tokio::test]
async fn snapshot_iface_list_json() {
    let s = render_iface_list(OutputFormat::Json, &RenderContext::default()).await;
    assert_snapshot!(s, @r#"
    [
      {
        "iface": "eth0",
        "kind": "ethernet",
        "state": "up",
        "mac": "aa:bb:cc:dd:ee:01",
        "carrier": true
      },
      {
        "iface": "wlan0",
        "kind": "wifi",
        "state": "connected",
        "mac": "aa:bb:cc:dd:ee:03",
        "carrier": true,
        "managed_profile": "/fi/nexus1/profile/wifi/X"
      },
      {
        "iface": "hci0",
        "kind": "bluetooth",
        "state": "powered",
        "mac": "dd:ee:ff:00:11:22",
        "carrier": false
      },
      {
        "iface": "/dev/gps0",
        "kind": "gnss",
        "state": "tracking",
        "mac": null,
        "carrier": false
      }
    ]
    "#);
}

#[tokio::test]
async fn snapshot_iface_list_pretty() {
    let s = render_iface_list(OutputFormat::Pretty, &RenderContext::default()).await;
    assert_snapshot!(s, @r"
    Interface:   eth0
    Kind:        ethernet
    State:       up
    MAC:         aa:bb:cc:dd:ee:01
    Carrier:     up

    Interface:   wlan0
    Kind:        wifi
    State:       connected
    MAC:         aa:bb:cc:dd:ee:03
    Carrier:     up

    Interface:   hci0
    Kind:        bluetooth
    State:       powered
    MAC:         dd:ee:ff:00:11:22
    Carrier:     down

    Interface:   /dev/gps0
    Kind:        gnss
    State:       tracking
    MAC:         —
    Carrier:     down
    ");
}

#[tokio::test]
async fn snapshot_status_human() {
    let s = render_status(OutputFormat::Human, &RenderContext::default()).await;
    assert_snapshot!(s, @r"
    Version:      0.1.0
    Power state:  active
    Interfaces:   4 (1 ethernet, 1 wifi, 1 bluetooth, 1 gnss)
    Profiles:     2 wifi, 1 ethernet, 0 bluetooth
    BlueZ:        reachable
    gpsd:         reachable
    Master key:   file
    Internet:     online
    Capabilities: events, properties
    ");
}

#[tokio::test]
async fn snapshot_status_terse() {
    let s = render_status(OutputFormat::Terse, &RenderContext::default()).await;
    assert_snapshot!(s, @"0.1.0:active:4:1:1:1:1:2:1:0:true:true:file");
}

#[tokio::test]
async fn snapshot_status_json() {
    let s = render_status(OutputFormat::Json, &RenderContext::default()).await;
    assert_snapshot!(s, @r#"
    {
      "version": "0.1.0",
      "power_state": "active",
      "api_capabilities": [
        "events",
        "properties"
      ],
      "interface_count": 4,
      "ethernet_count": 1,
      "wifi_count": 1,
      "bluetooth_count": 1,
      "gnss_count": 1,
      "wifi_profile_count": 2,
      "ethernet_profile_count": 1,
      "bluetooth_profile_count": 0,
      "master_key_source": "file",
      "bluez_available": true,
      "gpsd_available": true,
      "internet_connectivity": "internetOnline"
    }
    "#);
}
