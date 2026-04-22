# DD-001: Interface Discovery — Detailed Design

**Parent:** [Nexus Architecture](./nexus-architecture.md)
**Status:** Draft
**Scope:** Design of the Interface Monitor component — how Nexus discovers, classifies, and tracks all platform interfaces (Ethernet, Wi-Fi, Bluetooth, GNSS) from cold boot through runtime hotplug.

---

## Table of Contents

1. [Context](#1-context)
   - 1.1 [Repo Layout](#11-repo-layout)
2. [Discovery Sources](#2-discovery-sources)
3. [Netlink Basics (Overview)](#3-netlink-basics-overview)
4. [Socket Setup](#4-socket-setup)
   - 4.1 [rtnetlink Socket](#41-rtnetlink-socket)
   - 4.2 [nl80211 Generic Netlink Socket](#42-nl80211-generic-netlink-socket)
   - 4.3 [udev Monitor](#43-udev-monitor)
5. [Initial Enumeration (Cold Boot)](#5-initial-enumeration-cold-boot)
   - 5.1 [Step 1 — Dump rtnetlink Interfaces](#51-step-1--dump-rtnetlink-interfaces)
   - 5.2 [Step 2 — Probe nl80211 for Wireless Interfaces](#52-step-2--probe-nl80211-for-wireless-interfaces)
   - 5.3 [Step 3 — Dump PHY Capabilities](#53-step-3--dump-phy-capabilities)
   - 5.4 [Step 4 — Enumerate Bluetooth and GNSS via udev](#54-step-4--enumerate-bluetooth-and-gnss-via-udev)
   - 5.5 [Step 5 — Build the Interface Registry](#55-step-5--build-the-interface-registry)
   - 5.6 [Boot-Time Budget](#56-boot-time-budget)
6. [Runtime Hotplug and State Changes](#6-runtime-hotplug-and-state-changes)
   - 6.1 [rtnetlink Events](#61-rtnetlink-events)
   - 6.2 [Carrier Detection Subtleties](#62-carrier-detection-subtleties)
   - 6.3 [nl80211 Multicast Events](#63-nl80211-multicast-events)
   - 6.4 [udev Events](#64-udev-events)
7. [Classification State Machine](#7-classification-state-machine)
8. [Concurrency Model](#8-concurrency-model)
9. [Error Handling](#9-error-handling)
   - 9.1 [Netlink Message Drops](#91-netlink-message-drops)
   - 9.2 [Malformed Messages](#92-malformed-messages)
   - 9.3 [Forward Compatibility](#93-forward-compatibility)
   - 9.4 [Subsystem Unavailability](#94-subsystem-unavailability)
   - 9.5 [Observability](#95-observability)
10. [Configuration](#10-configuration)
11. [Testing Strategy](#11-testing-strategy)
12. [Implementation Phases](#12-implementation-phases)

**Appendix**

- [A. Netlink Reference](#appendix-a-netlink-reference) — Full protocol reference: framing, headers, attributes, multi-part dumps, errors

---

## 1. Context

The Interface Monitor is the single source of truth in Nexus for what interfaces exist and their current link-layer state. Every technology backend consumes events produced here. Getting discovery right is a prerequisite for everything else working, so this component is designed to be robust against:

- Interfaces appearing before their drivers finish registering (cold-boot race)
- Interfaces being renamed mid-startup (udev predictable-name assignment)
- Netlink message drops under load
- Subsystems (like nl80211) taking longer to come up than the kernel network layer
- USB adapters being plugged in and out during normal operation
- Kernel driver modules being unloaded and reloaded

For the architectural context, read [nexus-architecture.md](./nexus-architecture.md) first.

### 1.1 Repo Layout

The code for this component lives at:

```
crates/
  nexus-monitor/            <- the Interface Monitor component
    Cargo.toml
    src/
      lib.rs                <- re-exports, entry point (spawn_monitor_task)
      parse/                <- phase 1: netlink parsers
        mod.rs
        rtnl.rs             <- RTM_NEWLINK / RTM_DELLINK parsing
        nl80211.rs          <- nl80211 command parsing
        nla.rs              <- generic NLA walker (§9.3 + Appendix A.3)
        errors.rs           <- parse error types
      socket/               <- phase 2: socket wrappers over tokio::AsyncFd
        mod.rs
        rtnl.rs             <- RtnlSocket
        nl80211.rs          <- Nl80211McastSocket, Nl80211RequestSocket
        udev.rs             <- UdevMonitor
        ethtool.rs          <- ethtool_fd + SIOCETHTOOL poller
      genl.rs               <- phase 3: generic netlink family resolution
      enumerate.rs          <- phase 4: cold-boot enumeration
      events.rs             <- phase 5: NexusEvent emission helpers
      monitor.rs            <- phase 6: main tokio::select! loop
      classify.rs           <- phase 7: classification state machine
      registry.rs           <- in-memory InterfaceInfo registry
      metrics.rs            <- phase 9: metric declarations (§9.5)
      config.rs             <- configuration parsing (§10)
    tests/
      parse_golden.rs       <- hand-crafted byte-array tests (§11.1)
      hwsim_integration.rs  <- mac80211_hwsim tests (§11.2)

crates/
  nexus-core/               <- NexusEvent bus, shared types
    src/
      event.rs              <- NexusEvent enum (canonical definition)
      interface.rs          <- InterfaceInfo, InterfaceKind, OperState
```

Types that are consumed by multiple crates (`NexusEvent`, `InterfaceInfo`, `InterfaceKind`, `OperState`) live in `nexus-core`. The Interface Monitor *produces* events of these types but does not define them — the canonical definitions live in `nexus-core/src/event.rs` and `nexus-core/src/interface.rs`.

---

## 2. Discovery Sources

Nexus uses three discovery sources, each covering a different technology domain:

| Source | Socket / Interface | Discovers |
|---|---|---|
| rtnetlink (`NETLINK_ROUTE`) | Netlink | All network interfaces (Ethernet, Wi-Fi, virtual, loopback, etc.) and their link state |
| nl80211 (Generic Netlink) | Netlink | Wireless interfaces and PHY devices with full capability information |
| udev / sysfs | Netlink + filesystem | Bluetooth HCI adapters, GNSS serial/USB devices, and any device not exposed via network netlink |

Why three sources:

- **rtnetlink** is the authoritative source for network interfaces, but it can't distinguish Wi-Fi from Ethernet — both appear as `ARPHRD_ETHER`.
- **nl80211** provides the wireless classification and per-radio capabilities, but only covers wireless.
- **udev** is needed for Bluetooth and GNSS, which don't appear on network netlink at all.

---

## 3. Netlink Basics (Overview)

Nexus communicates with the kernel over two netlink protocols: **rtnetlink** (`NETLINK_ROUTE`) for network interface lifecycle and **generic netlink** (the `nl80211` family) for wireless specifics. Readers already fluent in netlink can skim this section and proceed to [Section 4](#4-socket-setup). For a complete protocol reference — message headers, attribute encoding, nesting rules, multi-part dumps, errors — see **[Appendix A: Netlink Reference](#appendix-a-netlink-reference)** at the end of this document.

Only the essentials needed to follow the rest of the design:

- Every netlink message starts with a 16-byte `nlmsghdr` containing length, type, flags, sequence, and sender port ID. Responses from the kernel carry `nlmsg_pid = 0`; replies are routed per-socket by the kernel, not by re-reading the request's pid (Appendix §A.1).
- After the outer header comes a protocol-specific fixed header (`ifinfomsg` for rtnetlink, `genlmsghdr` for generic netlink), then a chain of typed **netlink attributes** (NLAs). Attributes can be nested and must be walked with strict 4-byte alignment (Appendix §A.2–A.3).
- Dumps are **multi-part**: the kernel emits a stream of messages with `NLM_F_MULTI` set, terminated by an `NLMSG_DONE` message (Appendix §A.4).
- Errors arrive as `NLMSG_ERROR` with a negative errno payload. Common errors during discovery include `ENODEV`, `EBUSY`, `EOPNOTSUPP`, and `EPERM` (Appendix §A.5).

---

## 4. Socket Setup

Throughout this document, socket file descriptors are referred to by consistent names. For clarity:

| Name | Protocol | Purpose |
|---|---|---|
| `rtnl_fd` | `NETLINK_ROUTE` | rtnetlink events (link add/remove/state change) and on-demand `RTM_GETLINK` dumps |
| `nl80211_mcast_fd` | `NETLINK_GENERIC` (nl80211 family) | Subscription to nl80211 `"config"` and `"scan"` multicast groups |
| `nl80211_rr_fd` | `NETLINK_GENERIC` (nl80211 family) | nl80211 dumps (`GET_INTERFACE`, `GET_WIPHY`) and unicast probes |
| `udev_fd` | udev monitor | Hotplug events for `bluetooth` and `tty` subsystems |
| `ethtool_fd` | `AF_INET` `SOCK_DGRAM` | Carrier polling via `SIOCETHTOOL`/`ETHTOOL_GLINK` (opened once at startup) |

All netlink sockets are opened with `SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK`.

### 4.1 rtnetlink Socket

```rust
// Open the rtnetlink socket (NETLINK_ROUTE)
let rtnl_fd = socket(
    AF_NETLINK,
    SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK,
    NETLINK_ROUTE,
);

// Bind to multicast groups for ongoing events
let addr = sockaddr_nl {
    nl_family: AF_NETLINK as u16,
    nl_pad: 0,
    nl_pid: 0,              // kernel assigns a unique port ID
    nl_groups: RTMGRP_LINK, // (1 << (RTNLGRP_LINK - 1))
};
bind(rtnl_fd, &addr);

// Increase the receive buffer — default is 212KB, which is too small
// for a system with many interfaces generating rapid events
let rcvbuf_size: i32 = 1 * 1024 * 1024;  // 1 MiB
setsockopt(rtnl_fd, SOL_SOCKET, SO_RCVBUFFORCE, &rcvbuf_size);
// SO_RCVBUFFORCE requires CAP_NET_ADMIN but bypasses net.core.rmem_max.
// Fall back to SO_RCVBUF if CAP_NET_ADMIN is not available.

// Enable extended ACK reporting for better error diagnostics
let one: i32 = 1;
setsockopt(rtnl_fd, SOL_NETLINK, NETLINK_EXT_ACK, &one);

// Enable strict checking — rejects malformed requests with clearer errors
setsockopt(rtnl_fd, SOL_NETLINK, NETLINK_GET_STRICT_CHK, &one);
```

`RTMGRP_LINK` subscribes to `RTM_NEWLINK` and `RTM_DELLINK` messages for all network interfaces.

**Additional multicast groups to consider (future):**

- `RTMGRP_IPV4_IFADDR` — IP address changes on interfaces (not needed for discovery since we delegate IP to networkd, but useful for monitoring).
- `RTMGRP_NEIGH` — Neighbor (ARP) table changes.

For v0.1, Nexus only subscribes to `RTMGRP_LINK`.

### 4.2 nl80211 Generic Netlink Socket

Generic netlink family IDs are not fixed — they are assigned at module load time. Nexus must resolve them at runtime.

**Why Nexus uses two nl80211 sockets.** Nexus opens two separate generic netlink sockets for nl80211:

1. A **multicast subscription socket** that joins the `"config"` and `"scan"` multicast groups. This socket is read-only from the monitor's perspective — it receives asynchronous events.
2. A **request/response socket** used for targeted dumps (`NL80211_CMD_GET_INTERFACE`, `NL80211_CMD_GET_WIPHY`) and unicast probes (classify a specific ifindex).

The two sockets are needed because a single netlink socket cannot simultaneously receive multicast events and block waiting for a specific request's response without the two streams interleaving. Each socket has its own kernel port ID (`nlmsg_pid`), and the kernel routes responses back to the socket that sent the request. Keeping them separate means the classification probe in [Section 7](#7-classification-state-machine) can await its response cleanly while the multicast socket continues processing unrelated events.

```rust
// Helper that opens and binds one generic netlink socket.
// Multicast group membership is handled separately via NETLINK_ADD_MEMBERSHIP
// after the socket is bound.
fn open_genl_socket() -> Result<Fd> {
    let fd = socket(
        AF_NETLINK,
        SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK,
        NETLINK_GENERIC,
    );
    bind(fd, &sockaddr_nl { nl_pid: 0, nl_groups: 0, ..Default::default() });
    Ok(fd)
}

// Multicast subscription socket — joined to "config" and "scan" groups below.
let nl80211_mcast_fd = open_genl_socket()?;
// Request/response socket — used for dumps and unicast probes.
let nl80211_rr_fd = open_genl_socket()?;

// Step 1: Resolve the nl80211 family ID by sending CTRL_CMD_GETFAMILY
// to the generic netlink controller (fixed family ID 0x10 = GENL_ID_CTRL).
// This uses the request/response socket.
let request = build_genl_message(
    family_id = GENL_ID_CTRL,        // 0x10
    cmd = CTRL_CMD_GETFAMILY,        // 3
    flags = NLM_F_REQUEST,
    attrs = [
        NLA(CTRL_ATTR_FAMILY_NAME, "nl80211"),
    ],
);
send(nl80211_rr_fd, &request);

// Parse the response. Key attributes in the reply:
//   CTRL_ATTR_FAMILY_ID    (u16) — the nl80211 family ID to use going forward
//   CTRL_ATTR_MCAST_GROUPS (nested) — multicast groups this family defines

let response = recv(nl80211_rr_fd);
let family_id = parse_u16(response, CTRL_ATTR_FAMILY_ID);

// Step 2: Walk CTRL_ATTR_MCAST_GROUPS to find group IDs
// Structure: each entry is a nested NLA containing:
//   CTRL_ATTR_MCAST_GRP_NAME (string)
//   CTRL_ATTR_MCAST_GRP_ID   (u32)
let groups = parse_mcast_groups(response);
let config_group = groups["config"];  // interface add/remove, reg domain
let scan_group = groups["scan"];      // scan trigger/results/abort

// Step 3: Join multicast groups on the multicast subscription socket only
for group_id in [config_group, scan_group] {
    setsockopt(nl80211_mcast_fd, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &group_id);
}
```

**Peer validation.** When reading from either socket, Nexus validates the `nlmsg_pid` field of the outer `nlmsghdr`. The kernel uses `nlmsg_pid = 0` for all messages it originates (both unicast replies and multicast events). Any message with `nlmsg_pid != 0` came from another userspace process on a shared multicast group and is ignored with a warning logged.

**Why multicast group IDs aren't fixed:** In modern kernels (>= 3.13), generic netlink multicast groups are assigned dynamically when families register. The old mechanism of fixed group numbers was deprecated because it didn't scale. This means parsing `CTRL_ATTR_MCAST_GROUPS` is mandatory, not optional.

**Split dumps.** Some nl80211 responses (notably `NL80211_CMD_GET_WIPHY`) can be split across multiple messages when they exceed the netlink buffer size. The `NL80211_ATTR_SPLIT_WIPHY_DUMP` flag must be set in the request to signal the kernel that Nexus can handle split responses. Each message in the split carries the same wiphy index; the parser must merge attributes from all messages for a given wiphy before treating the data as complete.

### 4.3 udev Monitor

```rust
// Using the udev-rs crate (wraps libudev)
let monitor = udev::MonitorBuilder::new()?
    .match_subsystem("bluetooth")?   // HCI adapters: hci0, hci1, ...
    .match_subsystem("tty")?         // Serial GNSS devices: ttyUSB0, ttyACM0
    .match_subsystem("gps")?         // Some GNSS chipsets register here
    .listen()?;

let fd = monitor.as_raw_fd();
```

The monitor file descriptor is epoll-able and integrates with the async event loop alongside the netlink sockets.

**udev property filtering for GNSS.** Not every `tty` device is a GNSS receiver. Nexus identifies GNSS devices by checking udev properties:

- `ID_USB_DRIVER == "cdc_acm"` or `"pl2303"` or `"cp210x"` — common USB-serial chipsets
- `ID_MODEL` contains `"GPS"`, `"GNSS"`, or a known receiver name (`"u-blox"`, `"SiRF"`)
- A custom udev rule from the integrator sets `NEXUS_GNSS=1`

The integrator-supplied udev rule is the most reliable option for embedded deployments where the GNSS hardware is known in advance.

---

## 5. Initial Enumeration (Cold Boot)

On startup, Nexus performs a full dump of all existing interfaces before entering the event loop. The dump order matters because classification depends on correlating results across sources.

### 5.1 Step 1 — Dump rtnetlink Interfaces

Send `RTM_GETLINK` with `NLM_F_DUMP | NLM_F_REQUEST`:

```rust
let request = build_rtnl_message(
    msg_type = RTM_GETLINK,
    flags = NLM_F_REQUEST | NLM_F_DUMP,
    header = ifinfomsg {
        ifi_family: AF_UNSPEC,
        ..zeroed()
    },
    attrs = [],  // empty = dump all interfaces
);
send(rtnl_fd, &request);
```

The kernel responds with a multi-part message, one `RTM_NEWLINK` per interface. For each message, parse the `ifinfomsg` header and walk the attribute chain.

**Attributes used:**

| Attribute | Type | Purpose |
|---|---|---|
| `IFLA_IFNAME` | NUL-terminated string | Interface name (`eth0`, `wlp2s0`, etc.) |
| `IFLA_ADDRESS` | 6 bytes | MAC address |
| `IFLA_MTU` | u32 | Current MTU |
| `IFLA_OPERSTATE` | u8 | `IF_OPER_UP`, `IF_OPER_DOWN`, `IF_OPER_DORMANT`, etc. |
| `IFLA_CARRIER` | u32 | 1 = carrier present, 0 = no carrier |
| `IFLA_LINKINFO` | nested | Contains `IFLA_INFO_KIND` for virtual devices |
| `IFLA_LINK` | i32 | Underlying device ifindex (for vlans, macvlans) |
| `IFLA_PHYS_PORT_NAME` | string | Physical port name (for multi-port NICs) |

**Filtering rules:**

1. Skip `ifi_type != ARPHRD_ETHER`. This drops loopback (`ARPHRD_LOOPBACK = 772`), tunnels, CAN bus, infiniband, etc.
2. Walk `IFLA_LINKINFO` → `IFLA_INFO_KIND`. If present and value is one of `"veth"`, `"bridge"`, `"bond"`, `"vlan"`, `"macvlan"`, `"tun"`, `"tap"`, `"gre"`, `"ipip"`, `"sit"`, `"ip6tnl"`, skip. These are virtual/stacked devices Nexus does not manage in v0.1.
3. Keep everything else. These are candidate physical interfaces.

At this point, Nexus has a list of physical `ARPHRD_ETHER` interfaces, but cannot distinguish Ethernet from Wi-Fi.

### 5.2 Step 2 — Probe nl80211 for Wireless Interfaces

Send `NL80211_CMD_GET_INTERFACE` with `NLM_F_DUMP` on the request/response socket:

```rust
let request = build_genl_message(
    family_id = nl80211_family_id,
    cmd = NL80211_CMD_GET_INTERFACE,
    flags = NLM_F_REQUEST | NLM_F_DUMP,
    attrs = [],
);
send(nl80211_rr_fd, &request);
```

The kernel responds with one message per wireless interface. Each message contains:

| Attribute | Type | Purpose |
|---|---|---|
| `NL80211_ATTR_IFINDEX` | u32 | Matches the ifindex from rtnetlink |
| `NL80211_ATTR_IFNAME` | string | Interface name (matches rtnetlink) |
| `NL80211_ATTR_WIPHY` | u32 | Physical radio device index |
| `NL80211_ATTR_IFTYPE` | u32 | `NL80211_IFTYPE_STATION`, `_AP`, `_MONITOR`, etc. |
| `NL80211_ATTR_MAC` | 6 bytes | Interface MAC |
| `NL80211_ATTR_WDEV` | u64 | Wireless device ID (unique per wiphy) |
| `NL80211_ATTR_SSID` | bytes | Current SSID if associated |
| `NL80211_ATTR_CHANNEL_WIDTH` | u32 | Current channel width if associated |

**Classification:** For every ifindex that appears in both the rtnetlink dump and the nl80211 dump, classify as `InterfaceKind::Wireless`. Everything remaining from the rtnetlink dump is `InterfaceKind::Ethernet`.

This simple rule is sufficient at cold boot because both dumps complete fully before event processing begins — there is no race between discovery and classification. The more elaborate classification state machine in [Section 7](#7-classification-state-machine) handles runtime hotplug where rtnetlink and nl80211 events can arrive in arbitrary order.

### 5.3 Step 3 — Dump PHY Capabilities

Send `NL80211_CMD_GET_WIPHY` with `NLM_F_DUMP` and the split-dump flag:

```rust
let request = build_genl_message(
    family_id = nl80211_family_id,
    cmd = NL80211_CMD_GET_WIPHY,
    flags = NLM_F_REQUEST | NLM_F_DUMP,
    attrs = [
        NLA_FLAG(NL80211_ATTR_SPLIT_WIPHY_DUMP),
    ],
);
send(nl80211_rr_fd, &request);
```

**Per-wiphy attributes of interest:**

| Attribute | Type | Purpose |
|---|---|---|
| `NL80211_ATTR_WIPHY` | u32 | PHY index (key for merging split messages) |
| `NL80211_ATTR_WIPHY_NAME` | string | e.g., `"phy0"` |
| `NL80211_ATTR_SUPPORTED_IFTYPES` | nested | Modes supported (station, AP, mesh, ...) |
| `NL80211_ATTR_WIPHY_BANDS` | nested | Supported frequency bands, rates, HT/VHT/HE capabilities |
| `NL80211_ATTR_SUPPORTED_COMMANDS` | array of u32 | Which nl80211 commands this driver supports |
| `NL80211_ATTR_CIPHER_SUITES` | array of u32 | Supported encryption ciphers |
| `NL80211_ATTR_MAX_NUM_SCAN_SSIDS` | u32 | Max SSIDs per scan request |
| `NL80211_ATTR_SUPPORT_AP_UAPSD` | flag | AP UAPSD support |
| `NL80211_ATTR_ROAM_SUPPORT` | flag | Driver-level roaming support |
| `NL80211_ATTR_MAX_NUM_SCHED_SCAN_SSIDS` | u32 | Max SSIDs for scheduled scans |
| `NL80211_ATTR_MAX_SCHED_SCAN_IE_LEN` | u16 | Max IE length for scheduled scans |
| `NL80211_ATTR_FEATURE_FLAGS` | u32 | Bit flags for various features |
| `NL80211_ATTR_EXT_FEATURES` | byte array | Extended feature bitmap (newer flags) |

**Merging split messages.** Each message carries `NL80211_ATTR_WIPHY`. Group messages by wiphy index and merge their attributes into a single `PhyCapabilities` struct. A wiphy is considered complete when `NLMSG_DONE` is received.

Store `PhyCapabilities` in a per-wiphy map and associate each wireless interface with its wiphy's capabilities. The Wi-Fi backend consults this to know what features it can attempt.

### 5.4 Step 4 — Enumerate Bluetooth and GNSS via udev

```rust
let enumerator = udev::Enumerator::new()?;
enumerator.match_subsystem("bluetooth")?;
let bt_devices = enumerator.scan_devices()?;

for device in bt_devices {
    let hci_name = device.sysname().to_string_lossy().into_owned();  // "hci0"
    let hci_index = parse_hci_index(&hci_name);
    let bluez_path = format!("/org/bluez/{}", hci_name);

    // Verify presence on BlueZ D-Bus before registering
    if dbus_object_exists("org.bluez", &bluez_path).await? {
        let bt_address = read_bt_address(&device)?;
        registry.add(build_bt_interface_info(
            &hci_name,
            hci_index,
            bt_address,
            &bluez_path,
        ));
    } else {
        // BlueZ may still be starting up. Retry in the runtime loop
        // when NameOwnerChanged fires for org.bluez.
        pending_bluez.insert(hci_name);
    }
}

// Helper that constructs the full InterfaceInfo for a Bluetooth adapter.
// The ifindex field holds a synthesized identifier (see note below) since
// HCI adapters have no kernel ifindex.
//
// `bt_address` is typed as MacAddr — the shared 48-bit address type defined
// in DD-003 §4.2 and reused by DD-004. (Earlier drafts used a distinct
// BtAddr; the consolidation on MacAddr eliminated a type that wasn't
// pulling its weight, since Bluetooth addresses are MAC-family identifiers.)
fn build_bt_interface_info(
    hci_name: &str,
    hci_index: u32,
    bt_address: MacAddr,
    bluez_path: &str,
) -> InterfaceInfo {
    InterfaceInfo {
        ifindex: synthesize_ifindex(IdKind::Bluetooth, hci_index),
        ifname: hci_name.to_string(),
        mac: bt_address.0,
        mtu: 0,                    // not applicable for BT
        operstate: OperState::Up,  // BlueZ presence implies adapter is usable
        carrier: true,
        discovered_at: Instant::now(),
        kind: InterfaceKind::Bluetooth {
            hci_name: hci_name.to_string(),
            hci_index,
            bt_address,
            bluez_path: bluez_path.to_string(),
        },
    }
}
```

**Note on identifiers for non-network subsystems.** Bluetooth HCI adapters and GNSS devices do not have a kernel `ifindex`. Nexus uses a single `ifindex: u32` field on `InterfaceInfo` that is:

- The real kernel ifindex for `InterfaceKind::Ethernet` and `InterfaceKind::Wireless`.
- A synthesized value in a non-overlapping range (e.g., `0x8000_0000 | hci_index` for Bluetooth, `0x9000_0000 | seq` for GNSS) for `InterfaceKind::Bluetooth` and `InterfaceKind::Gnss`.

Kernel ifindex values are positive `int32` and in practice stay well below the high bit, so the `0x8000_0000`+ range is collision-free. The synthesized id is stable for the lifetime of the device's presence in the registry and is used as the key for all subsequent events. A helper `synthesize_ifindex(kind, raw)` centralizes the range-mapping so no code site does bit-twiddling inline.

Similar enumeration for GNSS devices under `tty` subsystem, filtered by the property rules in [Section 4.3](#43-udev-monitor).

### 5.5 Step 5 — Build the Interface Registry

```rust
struct InterfaceInfo {
    /// Unique identifier used as the key across all backends and events.
    /// For Ethernet and Wireless, this is the real kernel ifindex.
    /// For Bluetooth and GNSS, it is a synthesized value in a non-overlapping
    /// range (see §5.4 note) since those subsystems have no kernel ifindex.
    ifindex: u32,
    /// Human-readable name: "eth0", "wlp2s0", "hci0", "/dev/ttyUSB0".
    ifname: String,
    mac: [u8; 6],
    mtu: u32,
    operstate: OperState,
    carrier: bool,
    kind: InterfaceKind,
    discovered_at: Instant,
}

enum InterfaceKind {
    Ethernet,
    Wireless {
        wiphy: u32,
        wiphy_name: String,
        wdev: u64,
        iftype: Nl80211IfType,
        capabilities: Arc<PhyCapabilities>,
    },
    Bluetooth {
        hci_name: String,       // "hci0"
        hci_index: u32,
        bt_address: MacAddr,    // 48-bit; MacAddr from nexus-core (DD-003 §4.2)
        bluez_path: String,     // "/org/bluez/hci0"
    },
    Gnss {
        device_path: String,    // "/dev/ttyUSB0"
        gpsd_device: String,    // gpsd's device identifier
        vendor_model: Option<String>,
    },
}

enum OperState {
    Unknown,        // IF_OPER_UNKNOWN
    NotPresent,     // IF_OPER_NOTPRESENT
    Down,           // IF_OPER_DOWN
    LowerLayerDown, // IF_OPER_LOWERLAYERDOWN
    Testing,        // IF_OPER_TESTING
    Dormant,        // IF_OPER_DORMANT
    Up,             // IF_OPER_UP
}
```

The field is named `ifindex` to match the kernel term for network interfaces, even though for Bluetooth and GNSS its value comes from a synthesized id-space rather than `sys/class/net/*/ifindex`. The Event Bus uses this field as the correlating identifier in all events (`CarrierChanged { ifindex, up }`, `InterfaceRemoved { ifindex }`, etc.).

Once the registry is populated, emit `InterfaceDiscovered` events for each interface. The core state machine routes these events to the appropriate technology backend.

### 5.6 Boot-Time Budget

Target: complete initial enumeration within **1.5 seconds** on a typical embedded device (ARM Cortex-A53, 512 MB RAM, 4 physical interfaces).

Phases and whether they run serially or in parallel:

| Phase | Target | Runs | Notes |
|---|---|---|---|
| Socket setup + nl80211 family resolution | 50 ms | serial | One round trip to genl controller |
| rtnetlink dump | 100 ms | parallel with nl80211 dumps | Independent socket |
| nl80211 interface dump | 100 ms | serial (dump 1 of 2) on `nl80211_rr_fd` | Must complete before wiphy dump |
| nl80211 wiphy dump | 300 ms | serial (dump 2 of 2) on `nl80211_rr_fd` | Split-dump across many bands/rates |
| udev enumeration | 200 ms | parallel with netlink dumps | Filesystem + libudev |
| BlueZ D-Bus verification | 500 ms | after udev enumeration | Worst case if BlueZ is slow |
| Registry construction + event emission | 50 ms | after all dumps complete | In-process |

Critical path on the netlink side: socket setup (50) + interface dump (100) + wiphy dump (300) = **450 ms**.

Critical path on the udev/D-Bus side: udev enumeration (200) + BlueZ verification (500) = **700 ms**.

These run in parallel, so the overall critical path is max(450, 700) + 50 (setup) + 50 (registry build) = **800 ms**, leaving **~700 ms** of slack against the 1.5 s target. The slack absorbs variance from slower hardware, more interfaces, or slow BlueZ initialization.

If BlueZ or gpsd is unavailable, Nexus must not block on them. They are tracked as "pending subsystems" and re-checked when `NameOwnerChanged` indicates they have appeared.

---

## 6. Runtime Hotplug and State Changes

After initial enumeration, Nexus enters its async event loop and processes events from three sources concurrently.

### 6.1 rtnetlink Events

`RTM_NEWLINK` messages arrive for:

**New interface created.** ifindex not in registry. Parse as in the initial dump. Probe nl80211 to classify (see [Section 7](#7-classification-state-machine)). Emit `InterfaceDiscovered`.

**Interface state change.** ifindex already in registry. Parse `IFLA_OPERSTATE` and `IFLA_CARRIER`. Compare against stored state:

- If `IFLA_CARRIER` changed: emit `CarrierChanged { ifindex, up }`.
- If `IFLA_OPERSTATE` changed: emit `OperstateChanged { ifindex, state }`.
- If both changed: emit both, carrier first.

Note: `IFLA_CARRIER` is not always present in every `RTM_NEWLINK` message. Drivers emit it when carrier state changes; intermediate state-change messages (e.g., flag changes) may omit it. Treat absence as "no change" rather than "no carrier."

**Interface renamed.** Same ifindex, different `IFLA_IFNAME`. Update the registry. Emit an internal `InterfaceRenamed` event so the D-Bus service layer can update its object paths.

Rename events typically happen early in boot when udev/systemd applies predictable names (`eth0` → `enp2s0`, `wlan0` → `wlp3s0`). Nexus must handle renames gracefully — they are normal, not errors.

`RTM_DELLINK` messages arrive when an interface is destroyed. Emit `InterfaceRemoved { ifindex }`. The relevant backend is responsible for cleaning up its own state.

**How backends consume these events.** `CarrierChanged` and `OperstateChanged` are emitted by the Interface Monitor regardless of interface technology, but the two backends interpret them very differently:

- **Ethernet backend** ([DD-002](./dd-002-ethernet-backend.md)) treats `CarrierChanged` as the authoritative "link is physically up / down" signal. When carrier comes up, the backend either signals `LinkReady` immediately (no 802.1X) or starts authentication; when carrier drops, the backend signals `LinkLost`. `OperstateChanged` is observed but not used for primary lifecycle decisions.

- **Wi-Fi backend** ([DD-003](./dd-003-wifi-backend.md)) treats the supplicant's state machine (over D-Bus) as authoritative. `CarrierChanged` and `OperstateChanged` from the Interface Monitor are used only as secondary sanity checks — e.g., to detect a wedged state where the supplicant reports `completed` but the kernel reports no carrier. The Wi-Fi backend does not drive its lifecycle from these events.

The Interface Monitor does not attempt to hide this asymmetry — it emits the raw events and lets each backend apply the appropriate semantics.

### 6.2 Carrier Detection Subtleties

Some Ethernet PHYs on embedded boards do not toggle carrier when a cable is connected — they hold carrier always-up or always-down depending on the driver. To handle this, Nexus supports a per-interface configuration override:

```toml
# /etc/nexus/interfaces.d/eth0.toml
[interface]
name = "eth0"
carrier_detect = "ethtool_poll"  # netlink (default) | ethtool_poll | assume_up
poll_interval_ms = 1000
```

When set to `ethtool_poll`, Nexus periodically issues `ETHTOOL_GLINK` via ioctl as a fallback. The ioctl is issued on a dedicated socket `ethtool_fd` — an `AF_INET SOCK_DGRAM` socket opened once at startup specifically for `SIOCETHTOOL` traffic. Any socket would work; `AF_INET` is the conventional choice.

```rust
// Opened once at startup:
let ethtool_fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);

// Poll one interface:
let mut edata = ethtool_value { cmd: ETHTOOL_GLINK, data: 0 };
let mut ifr = ifreq {
    ifr_name: cstr("eth0"),
    ifr_data: &mut edata as *mut _ as *mut c_void,
};
ioctl(ethtool_fd, SIOCETHTOOL, &mut ifr);
// edata.data is 1 if link is up, 0 if down
```

The poll is driven by a `tokio::time::interval` that is multiplexed into the same `select!` loop as the netlink and udev sockets (see [Section 8](#8-concurrency-model)). A single poll timer per `ethtool_poll`-configured interface is added to the select arms; when the timer fires, the ioctl is issued from the monitor task. This keeps the single-task invariant intact — no additional tasks or locks are introduced to support polled carrier detection.

When set to `assume_up`, carrier is reported as always up. Useful for point-to-point links or test fixtures.

### 6.3 nl80211 Multicast Events

From the `"config"` multicast group:

**`NL80211_CMD_NEW_INTERFACE`** — A new wireless interface was created. Contains the same attributes as the dump response. Add to registry, emit `InterfaceDiscovered`.

**`NL80211_CMD_DEL_INTERFACE`** — A wireless interface was destroyed. Remove from registry, emit `InterfaceRemoved`.

**`NL80211_CMD_NEW_WIPHY`** — A new radio appeared (USB adapter plugged in, driver loaded). Issue a targeted `NL80211_CMD_GET_WIPHY` for this specific wiphy index (not a dump) to fetch capabilities. Store them and wait for associated interface events.

**`NL80211_CMD_DEL_WIPHY`** — Radio removed. All interfaces on this wiphy are considered gone (though `DEL_INTERFACE` typically arrives first).

**`NL80211_CMD_REG_CHANGE`** — Regulatory domain changed (e.g., country code set). Update stored channel/power limits if Nexus caches them. The Wi-Fi backend may need to re-evaluate which channels it can scan.

**`NL80211_CMD_REG_BEACON_HINT`** — Driver received a beacon that hints at regulatory information. Informational only.

From the `"scan"` multicast group:

**`NL80211_CMD_SCAN_ABORTED`** — A scan was canceled. The Wi-Fi backend should decide whether to retry.

**`NL80211_CMD_SCHED_SCAN_RESULTS`** — Scheduled scan results available.

Note: `NL80211_CMD_TRIGGER_SCAN` and `NL80211_CMD_NEW_SCAN_RESULTS` are received on this group too, but the Wi-Fi backend typically processes them via wpa_supplicant/iwd D-Bus signals rather than raw nl80211. The Interface Monitor ignores these and lets the Wi-Fi backend handle them through its own channel. See [DD-003: Wi-Fi Backend](./dd-003-wifi-backend.md) for the detailed design.

### 6.4 udev Events

Processed via the udev monitor socket:

**`add`, subsystem `bluetooth`.** New HCI adapter. Query BlueZ D-Bus for the corresponding `/org/bluez/hciN` object. If BlueZ doesn't know about it yet (BlueZ is still initializing), add to a pending set and retry when `NameOwnerChanged` fires for `org.bluez`, or on a periodic retry timer. Once found, register in the interface registry and emit `InterfaceDiscovered`.

**`remove`, subsystem `bluetooth`.** HCI adapter removed. Remove from registry, emit `InterfaceRemoved`.

**`add`, subsystem `tty`.** Potential GNSS device. Check udev properties against the rules in [Section 4.3](#43-udev-monitor). If identified as GNSS, inform gpsd via its control socket (or via D-Bus if gpsd is configured with the D-Bus API) and register.

**`remove`, subsystem `tty`.** Serial device removed. If it was in the registry as GNSS, remove it and emit `InterfaceRemoved`.

---

## 7. Classification State Machine

Every interface in Nexus transitions through a classification state machine before being handed to a technology backend. Network interfaces (discovered via rtnetlink) must go through classification because rtnetlink alone cannot distinguish Ethernet from Wi-Fi. Bluetooth and GNSS devices arrive through udev with distinct subsystem tags, so their path bypasses the `Classifying` state entirely.

```
  Network interface path              Non-network subsystem path
  (rtnetlink)                          (udev)

        RTM_NEWLINK                          udev add event
            │                                      │
            ▼                                      │
      ┌──────────┐                                 │
      │Discovered│                                 │
      └────┬─────┘                                 │
           │                                       │
           ▼                                       │
    ┌───────────┐                                  │
    │Classifying│── probe nl80211                  │
    └─────┬─────┘                                  │
          │                                        │
     ┌────┴─────┐                            ┌─────┴─────┐
     │          │                            │           │
     ▼          ▼                            ▼           ▼
┌────────┐ ┌────────┐                    ┌─────┐    ┌──────┐
│Ethernet│ │Wireless│                    │ BT  │    │ GNSS │
└────┬───┘ └────┬───┘                    └──┬──┘    └──┬───┘
     │          │                           │          │
     └──────────┴──────────┬────────────────┴──────────┘
                           │
                           ▼
                 ┌───────────────────┐
                 │    Registered     │
                 │ (backend attached)│
                 └─────────┬─────────┘
                           │
                  RTM_DELLINK or udev remove
                           │
                           ▼
                      ┌────────┐
                      │ Removed│
                      └────────┘
```

The `Classifying` state handles the rtnetlink/nl80211 ordering ambiguity — an `RTM_NEWLINK` might arrive before the nl80211 driver has finished registering. Nexus handles this as follows:

1. On `RTM_NEWLINK` for an unknown ifindex with `ARPHRD_ETHER`: enter `Classifying`. Immediately send a unicast `NL80211_CMD_GET_INTERFACE` with `NL80211_ATTR_IFINDEX` set, using the request/response socket (see [Section 4.2](#42-nl80211-generic-netlink-socket)).

2. On response:
   - If nl80211 returns interface details: classify as `Wireless`, dump its wiphy capabilities if not cached.
   - If nl80211 returns `-ENODEV` or `-ENOENT`: classify as `Ethernet`.
   - If nl80211 returns `-EOPNOTSUPP` (kernel built without nl80211 support, or module missing): classify as `Ethernet` and log a distinct "nl80211 unavailable" warning. Continue operating without wireless classification for the remainder of the Nexus lifetime, or until nl80211 becomes available (see [Section 9.4](#94-subsystem-unavailability)).
   - If `NL80211_CMD_NEW_INTERFACE` arrives via multicast for an ifindex in `Classifying`: classify as `Wireless` (the multicast arrived before the unicast response).

3. **Timeout.** If classification hasn't resolved within a configurable timeout (default **2 seconds**), default to `Ethernet`. This handles fullmac drivers that are slow to register with cfg80211.

4. **Late wireless registration.** If an interface classified as `Ethernet` later receives `NL80211_CMD_NEW_INTERFACE` (can happen with late-loading fullmac drivers), the Interface Monitor performs a clean two-step transition rather than an ad-hoc reclassification:
   - Emit `InterfaceRemoved { ifindex }`, giving the Ethernet backend a chance to run its normal cleanup path (detach from any auth backend, release resources). See [DD-002](./dd-002-ethernet-backend.md) for the Ethernet backend's removal handling.
   - Emit `InterfaceDiscovered(InterfaceInfo { ..., kind: Wireless { .. } })`, which the Wi-Fi backend picks up as a fresh interface.

   This keeps each backend simple — neither needs to know about reclassification — at the cost of tearing down any in-progress work on the Ethernet side. In practice the interface was almost certainly idle (carrier not yet established) at the moment of reclassification, so nothing is lost. If an 802.1X authentication were genuinely mid-flight, aborting it is the correct outcome since the interface is about to be reinterpreted.

For Bluetooth and GNSS, classification is not ambiguous — udev events carry the subsystem tag, so the appropriate `Bluetooth` or `Gnss` kind is assigned directly without a probing step.

---

## 8. Concurrency Model

The Interface Monitor runs as a single async task that multiplexes across all its file descriptors:

```rust
async fn interface_monitor_task(
    mut rtnl: RtnlSocket,              // rtnl_fd
    mut nl80211_mcast: Nl80211McastSocket,   // nl80211_mcast_fd
    mut nl80211_rr: Nl80211RequestSocket,    // nl80211_rr_fd
    mut udev_mon: UdevMonitor,         // udev_fd
    mut ethtool_pollers: EthtoolPollerSet,   // uses ethtool_fd
    mut cmd_rx: mpsc::Receiver<MonitorCommand>,
    event_tx: broadcast::Sender<NexusEvent>,
) -> Result<()> {
    loop {
        tokio::select! {
            msg = rtnl.recv() => {
                match msg? {
                    RtnlMessage::NewLink(m) => handle_newlink(m, &event_tx).await?,
                    RtnlMessage::DelLink(m) => handle_dellink(m, &event_tx).await?,
                    _ => {} // ignore other message types
                }
            }
            msg = nl80211_mcast.recv() => {
                match msg? {
                    Nl80211Message::NewInterface(m) => handle_new_wiface(m, &event_tx).await?,
                    Nl80211Message::DelInterface(m) => handle_del_wiface(m, &event_tx).await?,
                    Nl80211Message::NewWiphy(m)     => handle_new_wiphy(m, &mut nl80211_rr).await?,
                    Nl80211Message::DelWiphy(m)     => handle_del_wiphy(m).await?,
                    Nl80211Message::RegChange(m)    => handle_reg_change(m).await?,
                    _ => {}
                }
            }
            ev = udev_mon.recv() => {
                handle_udev_event(ev?, &event_tx).await?;
            }
            tick = ethtool_pollers.next_tick() => {
                let (ifindex, state) = poll_carrier_via_ethtool(tick.ifindex)?;
                maybe_emit_carrier_change(ifindex, state, &event_tx).await?;
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    MonitorCommand::ProbeInterface { ifindex, reply } => {
                        // Probe uses the request/response socket so its reply
                        // does not interleave with multicast events.
                        let result = probe_nl80211(&mut nl80211_rr, ifindex).await;
                        let _ = reply.send(result);
                    }
                    MonitorCommand::Shutdown => return Ok(()),
                }
            }
        }
    }
}
```

**Design rationale for a single task.** The Interface Monitor holds mutable state — the interface registry, pending classifications, wiphy capabilities cache — that must stay internally consistent. Putting all I/O into one task eliminates the need for locks on this state. The task is I/O-bound, so a single task easily handles the message volume on embedded hardware.

**Why two nl80211 sockets — the actual benefit.** As introduced in [Section 4.2](#42-nl80211-generic-netlink-socket), Nexus opens two nl80211 sockets: one for multicast event subscription, one for request/response. It is important to be precise about what having two sockets does and does not do for concurrency:

- **What it does:** Kernel-side receive buffers for the two sockets are independent. While the Interface Monitor is synchronously driving a probe on `nl80211_rr_fd` (send request, await response), the kernel continues to accept multicast events on `nl80211_mcast_fd` into that socket's buffer. When the probe completes and the select loop resumes, those queued events are drained. With a single shared socket, request responses and asynchronous multicast events would interleave in the same buffer — workable but harder to parse, and more prone to buffer pressure during dumps.

- **What it does *not* do:** Having two sockets does not let other `tokio::select!` arms run while the probe is in flight. When `probe_nl80211(...).await` is executing inside the `cmd_rx` arm, the outer `select!` is not polling other arms — the task is inside a single arm until that arm's code returns. Other arms resume being polled only after `probe_nl80211` completes and the loop iteration ends.

The net effect is that the two-socket design protects the multicast event stream from buffer pressure, but the single-task model means probes are effectively serialized against other select-arm work. This is acceptable because probes are bounded by a 2-second timeout and are rare relative to steady-state event volume.

**Ethtool poll timers.** For interfaces configured with `carrier_detect = "ethtool_poll"` (see [Section 6.2](#62-carrier-detection-subtleties)), a per-interface `tokio::time::interval` is registered with `EthtoolPollerSet`. The `next_tick()` method yields the ifindex of whichever timer fires first. The ioctl is issued from within the monitor task on `ethtool_fd` — no extra tasks or locks are introduced.

**Event bus isolation.** Events are sent via a broadcast channel. If a consumer is slow and lags, the channel returns `Lagged(n)` to that consumer but does not block the sender. The Interface Monitor must never be blocked by a downstream consumer — netlink message drops caused by slow consumers are far worse than any single consumer missing an event.

**Receive buffer sizing.** Netlink sockets can drop messages when the kernel-side buffer fills. Nexus requests a 1 MiB receive buffer via `SO_RCVBUFFORCE` (see [Section 4.1](#41-rtnetlink-socket)) and monitors for `ENOBUFS` errors on the socket. On `ENOBUFS`, the monitor triggers a full re-enumeration to recover any missed state.

---

## 9. Error Handling

### 9.1 Netlink Message Drops

Detected via one of:

- `recv()` returning `ENOBUFS` — the kernel's per-socket queue overflowed and at least one message was dropped. This is the primary signal on non-blocking netlink sockets.
- A dump response that never terminates with `NLMSG_DONE` within the timeout window — indicates the dump was truncated.
- `SO_RXQ_OVFL` socket option (if enabled) reporting a non-zero overflow counter in the control message.

Sequence number gaps within a single dump are *not* a reliable drop signal — dump responses share the request's sequence number across all reply parts, so gaps in seq are normal. Inter-dump seq progression is checked for request/response correlation only.

Recovery on drop:

1. Log the drop with socket name and current buffer size.
2. Trigger a full re-enumeration (dump rtnetlink and nl80211 again).
3. Diff against the current registry: add missing interfaces, update changed state, remove gone interfaces. Emit events for each delta.

Re-enumeration is expensive but rare. It is cheaper than the alternative of being in an inconsistent state.

### 9.2 Malformed Messages

If a netlink message fails to parse (wrong length, invalid attribute nesting, unknown required attribute):

1. Log the error with the raw bytes (at debug level) and parsed portion (at info level).
2. Skip the message and continue processing.
3. Increment a metric `nexus_netlink_parse_errors_total{socket=...}` for observability.

Do not panic. Do not close the socket. Netlink is a hot path and the parser must be tolerant of future kernel additions.

### 9.3 Forward Compatibility

New kernel versions may add new NLA types. The parser must skip unknown attributes rather than failing. Structure the parser as:

```rust
fn parse_attrs(buf: &[u8]) -> AttrMap {
    let mut map = AttrMap::new();
    let mut cursor = 0;
    while cursor + 4 <= buf.len() {
        let nla_len = u16::from_ne_bytes([buf[cursor], buf[cursor+1]]) as usize;
        let nla_type = u16::from_ne_bytes([buf[cursor+2], buf[cursor+3]]);

        // Reject implausible lengths.
        if nla_len < 4 {
            break; // malformed — nla_len must cover at least the header
        }
        // Ensure the payload fits within the buffer.
        let end = match cursor.checked_add(nla_len) {
            Some(e) if e <= buf.len() => e,
            _ => break, // truncated or length overflow
        };

        let payload = &buf[cursor + 4..end];
        map.insert(nla_type, payload.to_vec());  // store regardless of known/unknown

        // Advance to the next 4-byte-aligned position. Guard against the
        // padded advance overflowing or exceeding the buffer; if it does,
        // this was the final attribute and the loop terminates cleanly.
        let advance = (nla_len + 3) & !3;
        cursor = match cursor.checked_add(advance) {
            Some(c) => c,
            None => break,
        };
    }
    map
}
```

The typed accessor layer above `AttrMap` returns `None` for missing attributes and `Err` for type mismatches, but never panics.

In practice, kernel-emitted netlink messages are always properly padded — the message length reported in `nlmsg_len` accounts for padding bytes following the last attribute. The bounds-checking above is defensive hardening for malformed userspace senders (possible if a future nl80211 message is interleaved through a third-party process) and for fuzz testing.

### 9.4 Subsystem Unavailability

If nl80211 returns `EOPNOTSUPP` or the family cannot be resolved at startup, Wi-Fi is unavailable. The Interface Monitor logs a warning and continues — Ethernet, Bluetooth, and GNSS still work. Wireless interfaces that appear will be classified as `Ethernet` (since nl80211 probing fails).

**Recovery from degraded state.** The Interface Monitor retries nl80211 family resolution on a low-frequency timer (default: every 30 seconds) while in the degraded state. When resolution succeeds — e.g., the `cfg80211` module was loaded after Nexus started — the monitor:

1. Opens the two nl80211 sockets and joins the multicast groups (as in [Section 4.2](#42-nl80211-generic-netlink-socket)).
2. Performs a fresh `NL80211_CMD_GET_INTERFACE` dump.
3. For every ifindex in the dump that is currently registered as `Ethernet`, applies the late-wireless-registration transition described in [Section 7](#7-classification-state-machine) (emit `InterfaceRemoved` then `InterfaceDiscovered` with the `Wireless` kind).
4. Stops the retry timer.

If udev fails to initialize at startup, Bluetooth and GNSS are unavailable. The Interface Monitor logs and continues with network interfaces only. There is no automatic recovery — udev failures are typically terminal (the udev daemon has died or the `/dev` mount is broken). A Nexus restart is the expected remedy.

If rtnetlink fails at startup, Nexus cannot function. Log, emit a fatal event, and exit. systemd will restart.

### 9.5 Observability

The Interface Monitor exposes the following metrics for operational visibility. Metric names follow Prometheus conventions; the transport (OpenMetrics endpoint, OTLP, etc.) is a runtime concern outside this DD.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `nexus_interface_monitor_events_total` | counter | `source` (`rtnl`/`nl80211_mcast`/`udev`/`ethtool_poll`), `kind` | Events successfully parsed and emitted |
| `nexus_netlink_parse_errors_total` | counter | `socket` | Messages that failed to parse |
| `nexus_netlink_enobufs_total` | counter | `socket` | `ENOBUFS` occurrences on receive |
| `nexus_reenumeration_total` | counter | `reason` (`enobufs`/`startup`/`nl80211_recovered`) | Full re-enumeration cycles |
| `nexus_interfaces_registered` | gauge | `kind` (`ethernet`/`wireless`/`bluetooth`/`gnss`) | Current number of registered interfaces by kind |
| `nexus_interface_classify_duration_seconds` | histogram | — | Time from `Discovered` to final classification |
| `nexus_interface_classify_timeout_total` | counter | — | Classification timeouts (defaulted to Ethernet) |
| `nexus_interface_reclassified_total` | counter | `from_kind`, `to_kind` | Late reclassifications |
| `nexus_subsystem_available` | gauge | `subsystem` (`nl80211`/`udev`/`bluez`/`gpsd`) | 1 if available, 0 if degraded |
| `nexus_cold_boot_duration_seconds` | histogram | — | Time from startup to initial enumeration complete |

These metrics are the observable contract of this component. A monitoring dashboard built on these should be sufficient to diagnose common issues (message drops, stuck classifications, degraded subsystems) without SSH access to the device.

---

## 10. Configuration

The Interface Monitor reads configuration from `/etc/nexus/monitor.toml`:

```toml
[monitor]
# Socket receive buffer size. Larger values help on busy systems.
# Default: 1 MiB.
netlink_rcvbuf_bytes = 1048576

# Timeout for classification before defaulting to Ethernet.
# Default: 2s.
classify_timeout_ms = 2000

# Whether to use strict netlink checking.
# Default: true.
netlink_strict_check = true

[monitor.filters]
# Interface name patterns to ignore entirely.
# Useful for excluding known virtual or management interfaces.
ignore_name_patterns = ["docker*", "veth*", "br-*"]

# Virtual interface kinds to ignore.
# Default covers standard virtual types.
ignore_virtual_kinds = ["veth", "bridge", "bond", "vlan", "macvlan", "tun", "tap"]

[[monitor.interface_override]]
# Per-interface settings. Matched by name.
name = "eth0"
carrier_detect = "ethtool_poll"
poll_interval_ms = 1000

[[monitor.interface_override]]
name = "eth1"
carrier_detect = "assume_up"
```

Configuration is validated at startup. Invalid configuration causes a fatal error (not a silent fallback to defaults) to make misconfiguration visible.

---

## 11. Testing Strategy

### 11.1 Unit Tests

- **Netlink parser tests.** For each message type (`RTM_NEWLINK`, `NL80211_CMD_NEW_INTERFACE`, etc.), test parsing against hand-crafted byte arrays covering valid, truncated, and malformed inputs.
- **Classification tests.** Feed synthetic event sequences into the classification state machine and assert correct outcomes, including the reorder cases (nl80211 arriving before/after rtnetlink).
- **Attribute nesting tests.** Verify nested NLA parsing handles multiple levels, unknown attributes, and padding correctly.

### 11.2 Integration Tests

- **mac80211_hwsim.** Use the kernel's Wi-Fi simulation module to create virtual wireless interfaces. Run the Interface Monitor against them and verify discovery, state changes, and removal.
- **veth pairs.** Create veth pairs, verify they are filtered out by default and discoverable if filtering is disabled.
- **Real hardware matrix.** On CI, exercise against at least one mac80211 driver (iwlwifi or ath9k) and one fullmac driver (brcmfmac) via hardware lab or QEMU with passed-through devices.

### 11.3 Fault Injection

- **Drop netlink messages.** Mock the netlink socket to drop messages randomly and verify recovery via re-enumeration.
- **Delayed nl80211.** Insert delays between rtnetlink and nl80211 responses to verify the classification timeout and late-reclassification paths.
- **Partial dumps.** Truncate dump responses to verify partial-dump handling.

### 11.4 Performance Tests

- **Boot-time budget.** Measure cold-boot enumeration time on target hardware. Assert against the budget in [Section 5.6](#56-boot-time-budget).
- **Hotplug burst.** Plug/unplug a USB Wi-Fi adapter in a tight loop and verify the monitor keeps up without dropping events.

---

## 12. Implementation Phases

Suggested build order for a human or agent implementing this component. Each phase should be completed, reviewed, and merged before the next begins. The phase boundaries are natural checkpoints for verifying the design.

### Phase 1 — Parsers

Standalone, fully unit-testable. No I/O. Deliverable: `crates/nexus-monitor/src/parse/`.

- Generic NLA walker (§9.3 + Appendix §A.3). Handles nested attributes, alignment, bounds checks.
- `nlmsghdr` parser (Appendix §A.1). Validates length, extracts type/flags/seq/pid.
- `ifinfomsg` parser (Appendix §A.2). Extracts `ifi_type`, `ifi_index`, `ifi_flags`.
- nl80211 message parser — enough attribute handling to populate the dump-result struct for `NL80211_CMD_GET_INTERFACE` and `NL80211_CMD_GET_WIPHY`.
- `NLMSG_ERROR` / `NLMSG_DONE` handling (Appendix §A.4, §A.5).

**Exit criterion:** The unit test suite in §11.1 passes, including truncated-message and unknown-attribute cases. No networking code yet.

### Phase 2 — Socket Wrappers

Typed wrappers around `tokio::io::unix::AsyncFd` for each socket in the naming table at the start of §4.

- `RtnlSocket`: open, bind to `RTMGRP_LINK`, apply `SO_RCVBUFFORCE`, `NETLINK_EXT_ACK`, `NETLINK_GET_STRICT_CHK`.
- `Nl80211McastSocket` and `Nl80211RequestSocket`: thin wrappers over generic netlink sockets with typed `send`/`recv`.
- `UdevMonitor`: wrapper over the `udev` crate's monitor.
- `EthtoolPoller`: `AF_INET SOCK_DGRAM` socket + per-interface `tokio::time::interval`.

**Exit criterion:** Each socket type has a happy-path integration test (open, send, recv) against the real kernel. No business logic yet.

### Phase 3 — Generic Netlink Family Resolution

`crates/nexus-monitor/src/genl.rs`. Resolves the nl80211 family ID and multicast group IDs at runtime (§4.2). Handles `EOPNOTSUPP` and missing-family errors cleanly.

**Exit criterion:** On a host with `cfg80211` loaded, resolution succeeds and returns a valid family ID and `config`/`scan` group IDs. On a host without nl80211, it returns a typed "unavailable" error.

### Phase 4 — Cold-Boot Enumeration

`crates/nexus-monitor/src/enumerate.rs`. The dump-and-classify path from §5.

- rtnetlink dump, filter to physical ARPHRD_ETHER interfaces.
- nl80211 interface dump, classify.
- nl80211 wiphy dump with split-dump merging.
- udev enumeration for bluetooth + tty subsystems.
- BlueZ D-Bus verification for each HCI adapter (with timeout).
- Build the initial `InterfaceInfo` registry.

**Exit criterion:** On a host with multiple interface types, `enumerate()` returns a complete registry within the §5.6 budget. Tested with hwsim + veth + real hardware.

### Phase 5 — Event Emission

`crates/nexus-monitor/src/events.rs`. Helpers that turn parsed messages and registry changes into `NexusEvent` values and push them onto the broadcast bus.

- `emit_interface_discovered(info)`, `emit_carrier_changed(ifindex, up)`, etc.
- Deduplication logic (don't emit `CarrierChanged` if state didn't actually change).
- Backpressure handling (`broadcast::Sender::send` may return `SendError` if no receivers; treat as non-fatal).

**Exit criterion:** Unit tests verify correct event shapes and deduplication.

### Phase 6 — Main Loop

`crates/nexus-monitor/src/monitor.rs`. The `tokio::select!` task described in §8.

- Multiplexes rtnl, nl80211_mcast, nl80211_rr, udev, ethtool pollers, command channel.
- Calls through to phase 1 parsers on incoming messages.
- Calls phase 5 emitters on parsed events.
- Handles `MonitorCommand::ProbeInterface` and `MonitorCommand::Shutdown`.

**Exit criterion:** The task runs indefinitely against a live system, emitting events correctly as interfaces change.

### Phase 7 — Classification State Machine

`crates/nexus-monitor/src/classify.rs`. The state machine from §7.

- `Discovered → Classifying → {Ethernet, Wireless}` path with unicast probe and timeout.
- `EOPNOTSUPP` → Ethernet with distinct log.
- Late-wireless-registration: clean two-step (`InterfaceRemoved` + `InterfaceDiscovered`) per §7.
- For BT and GNSS, direct classification (no `Classifying` state).

**Exit criterion:** Unit tests exercise all reorder cases with synthetic event sequences. Integration test confirms reclassification works against a hot-plugged USB fullmac adapter.

### Phase 8 — Error Handling & Recovery

- `ENOBUFS` detection on recv, full re-enumeration with diff (§9.1).
- Malformed message handling: log, skip, increment metric (§9.2).
- Degraded-state recovery: retry timer for nl80211 family resolution (§9.4).

**Exit criterion:** Fault injection tests from §11.3 pass.

### Phase 9 — Metrics

`crates/nexus-monitor/src/metrics.rs`. Declare all counters, gauges, and histograms from §9.5. Wire them into the code paths above.

**Exit criterion:** Prometheus scrape returns all metrics from §9.5 with sensible values.

### Phase 10 — Integration Tests

Full integration coverage per §11.2 and §11.4.

- mac80211_hwsim test harness in CI.
- veth filtering test.
- Boot-time budget measurement on a reference embedded board.
- Hotplug burst test.

**Exit criterion:** CI green. Component is ready for DD-002 / DD-003 consumers.

### Parallel work across phases

Phases 1 (parsers), 2 (sockets), 3 (genl) can all proceed in parallel after the shared types in `nexus-core` are agreed. Phases 4 onward must be sequential because each builds on the last.

DD-002 and DD-003 consumers can begin stubbing against `nexus-core` types as soon as phase 5 (event emission) is stable.

---

## Appendix A: Netlink Reference

This appendix is the complete protocol reference for netlink as used by Nexus. The main body of the document assumes readers either know this or are willing to consult this appendix as needed.

### A.1 Message Framing

Every netlink message — rtnetlink, generic netlink, or otherwise — follows the same outer structure:

```
┌──────────────────────────────────────────────────────────┐
│ nlmsghdr (16 bytes)                                      │
│   nlmsg_len:  u32  — total message length                │
│   nlmsg_type: u16  — RTM_NEWLINK, genl family id, etc.   │
│   nlmsg_flags: u16 — NLM_F_MULTI, NLM_F_DUMP, etc.       │
│   nlmsg_seq:  u32  — sequence number                     │
│   nlmsg_pid:  u32  — sender port ID                      │
├──────────────────────────────────────────────────────────┤
│ Protocol-specific header                                 │
│   rtnetlink: ifinfomsg (16 bytes)                        │
│   genl:      genlmsghdr (4 bytes) → cmd + version        │
├──────────────────────────────────────────────────────────┤
│ Attributes (NLA chain)                                   │
│   ┌─ nlattr ─┐                                           │
│   │ nla_len  │ u16 — length including header             │
│   │ nla_type │ u16 — attribute type ID                   │
│   │ payload  │ variable — padded to 4-byte alignment     │
│   └──────────┘                                           │
│   ┌─ nlattr ─┐                                           │
│   │ ...      │                                           │
│   └──────────┘                                           │
└──────────────────────────────────────────────────────────┘
```

**Port IDs, sequence numbers, and routing.** The `nlmsg_pid` field in the outer header identifies the *sender's* netlink port ID. Each netlink socket gets a unique port ID from the kernel at `bind()` time (the user passes `nl_pid = 0` and the kernel assigns one; the assigned value is visible via `getsockname`). On outbound requests, userspace sets `nlmsg_pid` to its socket's port ID. On inbound messages from the kernel — both unicast replies and multicast events — `nlmsg_pid` is always `0`. Any inbound message with `nlmsg_pid != 0` originated from another userspace process on a shared multicast group; such messages are dropped with a warning.

The kernel delivers unicast replies back to the specific socket that sent the corresponding request, tracked internally by the kernel via the socket's bound port ID — not by re-reading `nlmsg_pid` from the request header. This means different sockets owned by the same process receive their own replies independently, which is the basis for using separate sockets for different response streams.

The `nlmsg_seq` field lets the sender correlate multi-part or asynchronous replies with specific requests. Nexus uses monotonically-increasing sequence numbers per socket.

### A.2 Protocol-Specific Headers

**The `ifinfomsg` header (rtnetlink):**

```c
struct ifinfomsg {
    ifi_family: u8,    // AF_UNSPEC
    ifi_type: u16,     // ARPHRD_ETHER (1), ARPHRD_LOOPBACK (772), etc.
    ifi_index: i32,    // unique interface index
    ifi_flags: u32,    // IFF_UP, IFF_RUNNING, IFF_LOWER_UP, etc.
    ifi_change: u32,   // change mask (usually 0xFFFFFFFF for full state)
}
```

Key `ifi_flags` bits for discovery:

| Flag | Meaning |
|---|---|
| `IFF_UP` | Interface is administratively up |
| `IFF_RUNNING` | Interface has resources allocated (driver loaded) |
| `IFF_LOWER_UP` | Physical link is up (the "carrier" signal for most drivers) |
| `IFF_DORMANT` | Interface is waiting for some operation (e.g., 802.1X) |
| `IFF_NOARP` | Interface does not support ARP (often set on PPP, some tunnels) |

**The `genlmsghdr` header (Generic Netlink):**

```c
struct genlmsghdr {
    cmd: u8,      // NL80211_CMD_GET_INTERFACE, etc.
    version: u8,  // protocol version (1 for nl80211)
    reserved: u16,
}
```

### A.3 Netlink Attributes (NLAs)

```c
struct nlattr {
    nla_len: u16,   // length including this header
    nla_type: u16,  // type ID; bit 15 = NLA_F_NESTED, bit 14 = NLA_F_NET_BYTEORDER
    // payload follows, padded to 4-byte alignment
}
```

**Nesting.** Some attributes contain nested NLA chains as their payload. For example, `NL80211_ATTR_WIPHY_BANDS` contains per-band attributes, which contain per-rate and per-frequency attributes. Nesting can go several levels deep. The `NLA_F_NESTED` flag (bit 15 of `nla_type`) indicates nesting, but some kernel subsystems don't set it consistently — Nexus must be prepared to parse nested payload based on the attribute type definition regardless of the flag.

**Alignment.** The `nla_len` field reports the unpadded length. The next NLA starts at `NLA_ALIGN(nla_len)` bytes later, where `NLA_ALIGN(len) = (len + 3) & ~3`. Parsers must respect this or they will walk off into garbage.

**Byte order.** Netlink payloads are in host byte order by default. The `NLA_F_NET_BYTEORDER` flag (bit 14) marks payloads that are in network byte order. In practice this is rare in the subsystems Nexus uses.

### A.4 Multi-Part Messages

Dump responses arrive as multiple netlink messages with `NLM_F_MULTI` set in `nlmsg_flags`. The sequence ends with a message of type `NLMSG_DONE`. Nexus must buffer and process all parts before treating the dump as complete.

### A.5 Errors

The kernel can respond with `NLMSG_ERROR` (type 2). The payload is:

```c
struct nlmsgerr {
    error: i32,      // negative errno, 0 = ack
    msg: nlmsghdr,   // the original request header
    // optionally followed by extended ack attributes
}
```

`error == 0` is used as an acknowledgment when `NLM_F_ACK` was requested. Negative errno values indicate actual errors. Common errors during discovery:

- `ENODEV` — Interface disappeared between dump and probe
- `EBUSY` — Interface busy, retry later
- `EOPNOTSUPP` — Driver doesn't support the requested command
- `EPERM` — Insufficient capabilities (need `CAP_NET_ADMIN`)

---

## Related Documents

- [Nexus Architecture](./nexus-architecture.md) — Parent architecture document
- [DD-002: Ethernet Backend](./dd-002-ethernet-backend.md) — Primary consumer of `CarrierChanged` events for wired lifecycle
- [DD-003: Wi-Fi Backend](./dd-003-wifi-backend.md) — Primary consumer of wireless interface events and PHY capabilities
- DD-004: Bluetooth Backend *(forthcoming)* — Will consume Bluetooth adapter events produced here
- DD-005: GNSS Backend *(forthcoming)* — Will consume GNSS device events produced here
