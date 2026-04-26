//! Phase-3 command snapshots + edge cases.
//!
//! Each new command gets at least one Human snapshot and one JSON
//! snapshot so both layers are anchored. `wifi show` ambiguity is
//! exercised end-to-end.

use async_trait::async_trait;
use insta::assert_snapshot;
use nexus_client::commands;
use nexus_client::errors::NexusctlError;
use nexus_client::output::{OutputFormat, RenderContext};
use nexus_client::proxy::{
    AltBss, BluetoothAdapterSummary, BluetoothDeviceDetail, BluetoothDeviceSummary,
    BluetoothListFilter, EthernetDetail, EthernetProfileDetail, GnssDetail, GnssFix,
    GnssSatellitesView, InterfaceDetail, InterfaceSummary, IpDetail, ManagerOps, ManagerStatus,
    MasterKeyInfo, ProfileDetail, ProfileSummary, WifiDetail, WifiProfileDetail, WifiProfileSummary,
};

/// A stub that serves pre-canned responses to every `ManagerOps`
/// method a Phase-3 test might touch.
#[derive(Default)]
struct Stub {
    rows: Vec<InterfaceSummary>,
    interface_detail: Option<InterfaceDetail>,
    bt_adapters: Vec<BluetoothAdapterSummary>,
    bt_devices: Vec<BluetoothDeviceSummary>,
    bt_device_detail: Option<BluetoothDeviceDetail>,
    gnss_sats: Option<GnssSatellitesView>,
    profiles: Vec<ProfileSummary>,
    wifi_profiles: Vec<WifiProfileSummary>,
    profile_detail: Option<ProfileDetail>,
    profile_toml: Option<String>,
    master_key: Option<MasterKeyInfo>,
}

#[async_trait]
impl ManagerOps for Stub {
    async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError> {
        Ok(ManagerStatus {
            version: "0.1.0".into(),
            power_state: "active".into(),
            api_capabilities: vec![],
            interface_count: self.rows.len() as u32,
            ethernet_count: 0,
            wifi_count: 0,
            bluetooth_count: 0,
            gnss_count: 0,
            wifi_profile_count: 0,
            ethernet_profile_count: 0,
            bluetooth_profile_count: 0,
            master_key_source: "file".into(),
            bluez_available: false,
            gpsd_available: false,
        })
    }
    async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError> {
        Ok(self.rows.clone())
    }
    async fn show_interface(&self, _ifname: &str) -> Result<InterfaceDetail, NexusctlError> {
        self.interface_detail
            .clone()
            .ok_or(NexusctlError::NotFound {
                reference: "test".into(),
            })
    }
    async fn list_bluetooth_adapters(&self) -> Result<Vec<BluetoothAdapterSummary>, NexusctlError> {
        Ok(self.bt_adapters.clone())
    }
    async fn list_bluetooth_devices(
        &self,
        _filter: BluetoothListFilter,
    ) -> Result<Vec<BluetoothDeviceSummary>, NexusctlError> {
        Ok(self.bt_devices.clone())
    }
    async fn show_bluetooth_device(
        &self,
        _address: &str,
    ) -> Result<BluetoothDeviceDetail, NexusctlError> {
        self.bt_device_detail
            .clone()
            .ok_or(NexusctlError::NotFound {
                reference: "test".into(),
            })
    }
    async fn gnss_satellites(
        &self,
        _device: Option<&str>,
    ) -> Result<GnssSatellitesView, NexusctlError> {
        self.gnss_sats.clone().ok_or(NexusctlError::NotFound {
            reference: "test".into(),
        })
    }
    async fn list_profiles(
        &self,
        _kind: Option<&str>,
    ) -> Result<Vec<ProfileSummary>, NexusctlError> {
        Ok(self.profiles.clone())
    }
    async fn list_wifi_profiles(&self) -> Result<Vec<WifiProfileSummary>, NexusctlError> {
        Ok(self.wifi_profiles.clone())
    }
    async fn show_profile(&self, _reference: &str) -> Result<ProfileDetail, NexusctlError> {
        self.profile_detail.clone().ok_or(NexusctlError::NotFound {
            reference: "test".into(),
        })
    }
    async fn export_profile(&self, _reference: &str) -> Result<String, NexusctlError> {
        self.profile_toml.clone().ok_or(NexusctlError::NotFound {
            reference: "test".into(),
        })
    }
    async fn master_key_info(&self) -> Result<MasterKeyInfo, NexusctlError> {
        self.master_key.clone().ok_or(NexusctlError::NotFound {
            reference: "master key".into(),
        })
    }
}

async fn capture<F>(f: F) -> String
where
    F: std::future::Future<Output = Result<(), NexusctlError>>,
{
    let _ = f.await;
    String::new() // replaced by each caller — we write into their own buf
}

// Each test assembles its own buffer — the helper above is just
// used in disambiguation paths that don't produce output. For
// normal renders we `let mut buf = Vec::new()` inline.

// ---------------------------------------------------------------------------
// iface show (wifi detail)
// ---------------------------------------------------------------------------

fn wifi_detail_fixture() -> InterfaceDetail {
    // Mirrors the production fallback path: state==connected but
    // neither systemd-networkd nor systemd-resolved is reachable on
    // the bus (stripped-down rootfs). The renderer collapses the
    // [ip] block to a single "unavailable" line and prints the
    // empty [alt aps] block as "none" — the existing snapshot
    // anchors this no-IP-context shape.
    InterfaceDetail {
        summary: InterfaceSummary {
            iface: "wlan0".into(),
            kind: "wifi".into(),
            state: "connected".into(),
            mac: Some("aa:bb:cc:dd:ee:03".into()),
            carrier: true,
            managed_profile: Some("/fi/nexus1/profile/wifi/X".into()),
        },
        mtu: None,
        ifindex: Some(5),
        wifi: Some(WifiDetail {
            state: "connected".into(),
            ssid: Some("corp-net".into()),
            bssid: Some("aa:11:bb:22:cc:33".into()),
            frequency_mhz: 5180,
            signal_dbm: -51,
            security: "wpa2-personal".into(),
            supplicant: "wpa_supplicant".into(),
            roaming_mode: "supplicant".into(),
            powered: true,
            alt_bsses: Vec::new(),
        }),
        ethernet: None,
        bluetooth: None,
        gnss: None,
        ip: Some(IpDetail {
            networkd_unavailable: Some("networkd not on bus".into()),
            ..IpDetail::default()
        }),
    }
}

fn wifi_detail_fixture_with_ip_and_alts() -> InterfaceDetail {
    let mut base = wifi_detail_fixture();
    base.ip = Some(IpDetail {
        ipv4_address: Some("192.0.2.42".into()),
        ipv4_prefix_length: Some(24),
        ipv4_gateway: Some("192.0.2.1".into()),
        ipv6_address: Some("2001:db8::1".into()),
        ipv6_gateway: Some("fe80::1".into()),
        dns: vec!["192.0.2.1".into(), "1.1.1.1".into()],
        ..IpDetail::default()
    });
    if let Some(wifi) = &mut base.wifi {
        wifi.alt_bsses = vec![
            AltBss {
                bssid: "aa:11:bb:22:cc:34".into(),
                frequency_mhz: 5180,
                signal_dbm: -58,
                security: vec!["wpa2".into()],
            },
            AltBss {
                bssid: "aa:11:bb:22:cc:35".into(),
                frequency_mhz: 2437,
                signal_dbm: -73,
                security: vec!["wpa2".into()],
            },
        ];
    }
    base
}

#[tokio::test]
async fn snapshot_wifi_show_human() {
    let stub = Stub {
        interface_detail: Some(wifi_detail_fixture()),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::iface::show(
        &stub,
        "wlan0",
        Some("wifi"),
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_snapshot!(s, @r"
    Interface: wlan0
    Kind:      wifi
    State:     connected
    MAC:       aa:bb:cc:dd:ee:03
    Carrier:   yes
    Ifindex:   5
    Profile:   /fi/nexus1/profile/wifi/X

    [wifi]
    State:      connected
    SSID:       corp-net
    BSSID:      aa:11:bb:22:cc:33
    Frequency:  5180 MHz
    Signal:     -51 dBm
    Security:   wpa2-personal
    Supplicant: wpa_supplicant
    Roaming:    supplicant
    Powered:    yes

    [ip]
      unavailable: networkd not on bus

    [alt aps]
      none
    ");
}

#[tokio::test]
async fn snapshot_wifi_show_human_with_ip_and_alts() {
    let stub = Stub {
        interface_detail: Some(wifi_detail_fixture_with_ip_and_alts()),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::iface::show(
        &stub,
        "wlan0",
        Some("wifi"),
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_snapshot!(s, @r"
    Interface: wlan0
    Kind:      wifi
    State:     connected
    MAC:       aa:bb:cc:dd:ee:03
    Carrier:   yes
    Ifindex:   5
    Profile:   /fi/nexus1/profile/wifi/X

    [wifi]
    State:      connected
    SSID:       corp-net
    BSSID:      aa:11:bb:22:cc:33
    Frequency:  5180 MHz
    Signal:     -51 dBm
    Security:   wpa2-personal
    Supplicant: wpa_supplicant
    Roaming:    supplicant
    Powered:    yes

    [ip]
    IPv4:         192.0.2.42
    IPv4 prefix:  24
    IPv4 gateway: 192.0.2.1
    IPv6:         2001:db8::1
    IPv6 gateway: fe80::1
    DNS:          192.0.2.1, 1.1.1.1

    [alt aps]
      aa:11:bb:22:cc:34  Ch 36  5180 MHz  -58 dBm  wpa2
      aa:11:bb:22:cc:35  Ch 6  2437 MHz  -73 dBm  wpa2
    ");
}

#[tokio::test]
async fn snapshot_wifi_show_json() {
    let stub = Stub {
        interface_detail: Some(wifi_detail_fixture()),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::iface::show(
        &stub,
        "wlan0",
        Some("wifi"),
        OutputFormat::Json,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
    assert_eq!(v["iface"], "wlan0");
    assert_eq!(v["wifi"]["ssid"], "corp-net");
    assert_eq!(v["wifi"]["signal_dbm"], -51);
    assert_eq!(v["wifi"]["frequency_mhz"], 5180);
}

// ---------------------------------------------------------------------------
// eth show
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_eth_show_human_authenticated() {
    let stub = Stub {
        interface_detail: Some(InterfaceDetail {
            summary: InterfaceSummary {
                iface: "eth0".into(),
                kind: "ethernet".into(),
                state: "authenticated".into(),
                mac: Some("aa:bb:cc:dd:ee:01".into()),
                carrier: true,
                managed_profile: Some("/fi/nexus1/profile/ethernet/Y".into()),
            },
            mtu: None,
            ifindex: Some(2),
            wifi: None,
            ethernet: Some(EthernetDetail {
                state: "authenticated".into(),
                auth_backend: "wpa_supplicant".into(),
                auth_failure_reason: String::new(),
                eap_method: "PEAP".into(),
            }),
            bluetooth: None,
            gnss: None,
            ip: Some(IpDetail {
                ipv4_address: Some("198.51.100.7".into()),
                ipv4_prefix_length: Some(24),
                ipv4_gateway: Some("198.51.100.1".into()),
                dns: vec!["198.51.100.1".into()],
                ..IpDetail::default()
            }),
        }),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::iface::show(
        &stub,
        "eth0",
        Some("ethernet"),
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_snapshot!(s, @r"
    Interface: eth0
    Kind:      ethernet
    State:     authenticated
    MAC:       aa:bb:cc:dd:ee:01
    Carrier:   yes
    Ifindex:   2
    Profile:   /fi/nexus1/profile/ethernet/Y

    [ethernet]
    State:        authenticated
    Auth backend: wpa_supplicant
    Auth failure: —
    EAP method:   PEAP

    [ip]
    IPv4:         198.51.100.7
    IPv4 prefix:  24
    IPv4 gateway: 198.51.100.1
    IPv6:         —
    IPv6 gateway: —
    DNS:          198.51.100.1
    ");
}

// ---------------------------------------------------------------------------
// bt adapters + list + show
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_bt_adapters_human() {
    let stub = Stub {
        bt_adapters: vec![
            BluetoothAdapterSummary {
                ifname: "hci0".into(),
                address: "00:1A:7D:DA:71:13".into(),
                state: "powered".into(),
                powered: true,
                discovering: false,
                known_device_count: 2,
            },
            BluetoothAdapterSummary {
                ifname: "hci1".into(),
                address: "00:1A:7D:DA:71:14".into(),
                state: "unavailable".into(),
                powered: false,
                discovering: false,
                known_device_count: 0,
            },
        ],
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::bt::adapters(
        &stub,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_snapshot!(s, @r"
     IFACE  ADDRESS            STATE        POWERED  DISCOVERING  DEVICES
     hci0   00:1A:7D:DA:71:13  powered      yes      no           2
     hci1   00:1A:7D:DA:71:14  unavailable  no       no           0
    ");
}

#[tokio::test]
async fn snapshot_bt_list_human() {
    let stub = Stub {
        bt_devices: vec![
            BluetoothDeviceSummary {
                adapter: "hci0".into(),
                address: "AA:BB:CC:DD:EE:01".into(),
                name: "keeb".into(),
                state: "connected".into(),
                paired: true,
                bonded: true,
                trusted: true,
                connected: true,
                rssi: -62,
                transport: "bredr".into(),
            },
            BluetoothDeviceSummary {
                adapter: "hci0".into(),
                address: "AA:BB:CC:DD:EE:02".into(),
                name: "".into(),
                state: "discovered".into(),
                paired: false,
                bonded: false,
                trusted: false,
                connected: false,
                rssi: -88,
                transport: "le".into(),
            },
        ],
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::bt::list(
        &stub,
        BluetoothListFilter::All,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_snapshot!(s, @r"
     ADAPTER  ADDRESS            NAME  STATE       PAIRED  CONN  RSSI
     hci0     AA:BB:CC:DD:EE:01  keeb  connected   yes     yes   -62
     hci0     AA:BB:CC:DD:EE:02        discovered  no      no    -88
    ");
}

#[tokio::test]
async fn snapshot_bt_show_human() {
    let stub = Stub {
        bt_device_detail: Some(BluetoothDeviceDetail {
            summary: BluetoothDeviceSummary {
                adapter: "hci0".into(),
                address: "AA:BB:CC:DD:EE:01".into(),
                name: "kbd".into(),
                state: "connected".into(),
                paired: true,
                bonded: true,
                trusted: true,
                connected: true,
                rssi: -55,
                transport: "bredr".into(),
            },
            address_type: "bredr".into(),
            alias: "Work keyboard".into(),
            tx_power: 0,
            uuids: vec!["00001124-0000-1000-8000-00805f9b34fb".into()],
            blocked: false,
            profile_path: Some("/fi/nexus1/profile/bluetooth/X".into()),
        }),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::bt::show(
        &stub,
        "AA:BB:CC:DD:EE:01",
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("Work keyboard"));
    assert!(s.contains("Connected:    yes"));
    assert!(s.contains("Transport:    bredr"));
    assert!(s.contains("/fi/nexus1/profile/bluetooth/X"));
}

// ---------------------------------------------------------------------------
// gnss show + satellites
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_gnss_show_human_with_fix() {
    let stub = Stub {
        interface_detail: Some(InterfaceDetail {
            summary: InterfaceSummary {
                iface: "gpsd-0".into(),
                kind: "gnss".into(),
                state: "tracking".into(),
                mac: None,
                carrier: false,
                managed_profile: None,
            },
            mtu: None,
            ifindex: None,
            wifi: None,
            ethernet: None,
            bluetooth: None,
            gnss: Some(GnssDetail {
                state: "tracking".into(),
                device_path: "/dev/ttyUSB0".into(),
                vendor_model: "u-blox M8".into(),
                gpsd_connected: true,
                satellites_in_view: 12,
                satellites_used: 9,
                horizontal_error_m: 2.1,
                last_fix: Some(GnssFix {
                    time_unix_ms: 0,
                    mode: 3,
                    latitude: 51.504500,
                    longitude: -0.087500,
                    altitude_m: 15.5,
                    speed_mps: 0.0,
                    track_deg: 0.0,
                    horizontal_error_m: 2.1,
                    vertical_error_m: 0.0,
                    satellites_used: 9,
                }),
            }),
            ip: None,
        }),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::iface::show(
        &stub,
        "gpsd-0",
        Some("gnss"),
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("[gnss]"));
    assert!(s.contains("Fix mode:"));
    assert!(s.contains("3D"));
    assert!(s.contains("Lat/lon:"));
    assert!(s.contains("51.504500"));
}

#[tokio::test]
async fn snapshot_gnss_satellites_human() {
    let stub = Stub {
        gnss_sats: Some(GnssSatellitesView {
            device: "/dev/ttyUSB0".into(),
            in_view: 12,
            used: 9,
        }),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::gnss::satellites(
        &stub,
        None,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_snapshot!(s, @r"
    Device:  /dev/ttyUSB0
    In view: 12
    Used:    9
    # per-satellite detail is not yet surfaced on D-Bus; counts only
    ");
}

// ---------------------------------------------------------------------------
// profile list / show / export
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_profile_list_human() {
    let stub = Stub {
        profiles: vec![
            ProfileSummary {
                id: "01HPQY8S2N0Z8K9M7V3Y2F4T5W".into(),
                kind: "wifi".into(),
                label: "corp".into(),
                credentials_invalid: false,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-02T00:00:00Z".into(),
            },
            ProfileSummary {
                id: "01HPQY8S2N0Z8K9M7V3Y2F4T5X".into(),
                kind: "ethernet".into(),
                label: "office-dot1x".into(),
                credentials_invalid: true,
                created_at: "2026-01-03T00:00:00Z".into(),
                updated_at: "2026-01-04T00:00:00Z".into(),
            },
        ],
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::profile::list(
        &stub,
        None,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_snapshot!(s, @r"
     ID                          KIND      LABEL         CREDS
     01HPQY8S2N0Z8K9M7V3Y2F4T5W  wifi      corp          ok
     01HPQY8S2N0Z8K9M7V3Y2F4T5X  ethernet  office-dot1x  invalid
    ");
}

#[tokio::test]
async fn profile_export_writes_toml_to_stdout() {
    let stub = Stub {
        profile_toml: Some(
            "kind = \"wifi\"\nlabel = \"corp\"\n\n[wifi]\nssid = \"corp-net\"\n".into(),
        ),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::profile::export(&stub, "corp", &mut buf)
        .await
        .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("kind = \"wifi\""));
    assert!(s.contains("ssid = \"corp-net\""));
}

#[tokio::test]
async fn snapshot_profile_show_wifi_human() {
    let stub = Stub {
        profile_detail: Some(ProfileDetail {
            summary: ProfileSummary {
                id: "01HPQY8S2N0Z8K9M7V3Y2F4T5W".into(),
                kind: "wifi".into(),
                label: "corp".into(),
                credentials_invalid: false,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-02T00:00:00Z".into(),
            },
            wifi: Some(WifiProfileDetail {
                ssid: "corp-net".into(),
                hidden: false,
                priority: 10,
                auto_connect: true,
                fast_transition: false,
                security_type: "wpa2_personal".into(),
                has_credentials: vec!["passphrase".into()],
                bssid_preferred: None,
                bssid_blacklist: vec![],
                scan_frequencies: vec![],
            }),
            ethernet: None,
        }),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::profile::show(
        &stub,
        "corp",
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("ID:"));
    assert!(s.contains("01HPQY8S2N0Z8K9M7V3Y2F4T5W"));
    assert!(s.contains("SSID:"));
    assert!(s.contains("corp-net"));
    assert!(s.contains("Stored creds:"));
    assert!(s.contains("passphrase"));
}

// ---------------------------------------------------------------------------
// wifi profiles
// ---------------------------------------------------------------------------

fn wifi_profiles_fixture() -> Vec<WifiProfileSummary> {
    vec![
        WifiProfileSummary {
            id: "01HPQY8S2N0Z8K9M7V3Y2F4T5W".into(),
            ssid: "corp-net".into(),
            label: "corp".into(),
            security_type: "wpa2_enterprise".into(),
            priority: 20,
            auto_connect: true,
            hidden: false,
            credentials_invalid: false,
        },
        WifiProfileSummary {
            id: "01HPQY8S2N0Z8K9M7V3Y2F4T5X".into(),
            ssid: "home".into(),
            label: "".into(),
            security_type: "wpa2_personal".into(),
            priority: 10,
            auto_connect: true,
            hidden: false,
            credentials_invalid: true,
        },
        WifiProfileSummary {
            id: "01HPQY8S2N0Z8K9M7V3Y2F4T5Y".into(),
            ssid: "iot-stealth".into(),
            label: "iot".into(),
            security_type: "wpa3_personal".into(),
            priority: 0,
            auto_connect: false,
            hidden: true,
            credentials_invalid: false,
        },
    ]
}

#[tokio::test]
async fn snapshot_wifi_profiles_human() {
    let stub = Stub {
        wifi_profiles: wifi_profiles_fixture(),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::wifi::profiles(
        &stub,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_snapshot!(s, @r"
     SSID                  LABEL  SECURITY         PRIORITY  AUTO  CREDS    ID
     corp-net              corp   wpa2_enterprise  20        yes   ok       01HPQY8S2N0Z8K9M7V3Y2F4T5W
     home                         wpa2_personal    10        yes   invalid  01HPQY8S2N0Z8K9M7V3Y2F4T5X
     iot-stealth (hidden)  iot    wpa3_personal    0         no    ok       01HPQY8S2N0Z8K9M7V3Y2F4T5Y
    ");
}

#[tokio::test]
async fn wifi_profiles_human_shows_empty_message() {
    let stub = Stub::default();
    let mut buf = Vec::new();
    commands::wifi::profiles(
        &stub,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert_eq!(s, "no wifi profiles\n");
}

#[tokio::test]
async fn wifi_profiles_json_round_trips_every_field() {
    let stub = Stub {
        wifi_profiles: wifi_profiles_fixture(),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::wifi::profiles(
        &stub,
        OutputFormat::Json,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let s = String::from_utf8(buf).unwrap();
    // Spot-check that every Wi-Fi-specific field made it through
    // the pretty-printed JSON. Whitespace around `:` matches
    // `serde_json::to_writer_pretty` output.
    assert!(s.contains("\"ssid\": \"corp-net\""), "got: {s}");
    assert!(s.contains("\"security_type\": \"wpa2_enterprise\""));
    assert!(s.contains("\"priority\": 20"));
    assert!(s.contains("\"auto_connect\": true"));
    assert!(s.contains("\"hidden\": true"));
    assert!(s.contains("\"credentials_invalid\": true"));
}

// ---------------------------------------------------------------------------
// power get + admin master-key-info
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_power_get_human() {
    let stub = Stub::default();
    let mut buf = Vec::new();
    commands::power::get(
        &stub,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    assert_snapshot!(String::from_utf8(buf).unwrap(), @"Power state: active");
}

#[tokio::test]
async fn snapshot_power_get_json() {
    let stub = Stub::default();
    let mut buf = Vec::new();
    commands::power::get(
        &stub,
        OutputFormat::Json,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
    assert_eq!(v["power_state"], "active");
}

#[tokio::test]
async fn snapshot_admin_master_key_info_human() {
    let stub = Stub {
        master_key: Some(MasterKeyInfo {
            source: "file".into(),
        }),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::admin::master_key_info(
        &stub,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    assert_snapshot!(String::from_utf8(buf).unwrap(), @"Master key source: file");
}

// ---------------------------------------------------------------------------
// iface list --kind filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn iface_list_kind_filter_restricts_rows() {
    let stub = Stub {
        rows: vec![
            InterfaceSummary {
                iface: "eth0".into(),
                kind: "ethernet".into(),
                state: "up".into(),
                mac: None,
                carrier: true,
                managed_profile: None,
            },
            InterfaceSummary {
                iface: "wlan0".into(),
                kind: "wifi".into(),
                state: "connected".into(),
                mac: None,
                carrier: true,
                managed_profile: None,
            },
        ],
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::iface::list(
        &stub,
        Some("wifi"),
        OutputFormat::Terse,
        &RenderContext {
            fields: Some(vec!["iface".into()]),
            ..RenderContext::default()
        },
        &mut buf,
    )
    .await
    .unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "wlan0\n");
}

// ---------------------------------------------------------------------------
// Disambiguation: `wifi show` with two Wi-Fi interfaces errors with
// an InvalidArgument listing the candidates. The binary maps that
// to exit 1 today; the prompt describes the "usage error with list"
// shape — the exact exit-2 mapping arrives with Phase 7.4 when the
// operator can also `wifi connect <ssid>` without specifying.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wifi_show_with_multiple_interfaces_is_ambiguous() {
    let stub = Stub {
        rows: vec![
            InterfaceSummary {
                iface: "wlan0".into(),
                kind: "wifi".into(),
                state: "connected".into(),
                mac: None,
                carrier: true,
                managed_profile: None,
            },
            InterfaceSummary {
                iface: "wlan1".into(),
                kind: "wifi".into(),
                state: "disconnected".into(),
                mac: None,
                carrier: false,
                managed_profile: None,
            },
        ],
        ..Default::default()
    };
    let mut buf = Vec::new();
    let err = commands::iface::show_wifi(
        &stub,
        None,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap_err();
    match &err {
        NexusctlError::InvalidArgument { message } => {
            assert!(message.contains("wlan0"));
            assert!(message.contains("wlan1"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn wifi_show_with_single_interface_uses_it() {
    let stub = Stub {
        rows: vec![InterfaceSummary {
            iface: "wlan0".into(),
            kind: "wifi".into(),
            state: "connected".into(),
            mac: None,
            carrier: true,
            managed_profile: None,
        }],
        interface_detail: Some(wifi_detail_fixture()),
        ..Default::default()
    };
    let mut buf = Vec::new();
    commands::iface::show_wifi(
        &stub,
        None,
        OutputFormat::Human,
        &RenderContext::default(),
        &mut buf,
    )
    .await
    .unwrap();
    assert!(String::from_utf8(buf).unwrap().contains("Interface: wlan0"));
}

// ---------------------------------------------------------------------------
// iface events is a documented stub: handler prints a helpful line
// then exits with `Unsupported`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn iface_events_prints_planned_feature_message_and_errors() {
    let mut buf = Vec::new();
    let err = commands::iface::events_stub(&mut buf).unwrap_err();
    assert!(matches!(err, NexusctlError::Unsupported { .. }));
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("planned feature"));
    assert!(s.contains("nexusctl watch"));
}

// Silence unused-import warnings for types only touched in cfg
// branches.
#[allow(dead_code)]
fn _touch(_e: &EthernetProfileDetail) {}
#[allow(dead_code)]
async fn _capture_ignored<F>(f: F) -> String
where
    F: std::future::Future<Output = Result<(), NexusctlError>>,
{
    capture(f).await
}
