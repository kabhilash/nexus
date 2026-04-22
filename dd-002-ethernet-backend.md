# DD-002: Ethernet Backend — Detailed Design

**Parent:** [Nexus Architecture](./nexus-architecture.md)
**Depends on:** [DD-001: Interface Discovery](./dd-001-interface-discovery.md)
**Status:** Draft
**Scope:** Design of the Ethernet Backend — lifecycle management for wired interfaces, carrier tracking, and optional IEEE 802.1X port authentication via a pluggable auth backend.

---

## Table of Contents

1. [Context](#1-context)
   - 1.1 [Repo Layout](#11-repo-layout)
2. [Responsibilities](#2-responsibilities)
3. [Interface Lifecycle](#3-interface-lifecycle)
   - 3.1 [States](#31-states)
   - 3.2 [State Transitions](#32-state-transitions)
   - 3.3 [Core Lifecycle Logic](#33-core-lifecycle-logic)
4. [IEEE 802.1X Authentication](#4-ieee-8021x-authentication)
   - 4.1 [Protocol Overview](#41-protocol-overview)
   - 4.2 [Nexus's Role](#42-nexuss-role)
   - 4.3 [Authentication Flow (Wired EAP-TLS Example)](#43-authentication-flow-wired-eap-tls-example)
5. [Pluggable Auth Backend](#5-pluggable-auth-backend)
   - 5.1 [Trait Definition](#51-trait-definition)
   - 5.2 [Configuration Struct](#52-configuration-struct)
   - 5.3 [Backend Selection](#53-backend-selection)
   - 5.4 [Default Choice](#54-default-choice)
6. [wpa_supplicant Auth Backend](#6-wpa_supplicant-auth-backend)
   - 6.1 [D-Bus Service](#61-d-bus-service)
   - 6.2 [Attach](#62-attach)
   - 6.3 [Authenticate](#63-authenticate)
   - 6.4 [State Tracking](#64-state-tracking)
   - 6.5 [Detach](#65-detach)
7. [ead Auth Backend](#7-ead-auth-backend)
   - 7.1 [D-Bus Service](#71-d-bus-service)
   - 7.2 [Attach and Authenticate](#72-attach-and-authenticate)
   - 7.3 [Limitations](#73-limitations)
8. [Configuration](#8-configuration)
   - 8.1 [Global Configuration](#81-global-configuration)
   - 8.2 [Per-Interface Profile](#82-per-interface-profile)
9. [Error Handling](#9-error-handling)
   - 9.1 [Auth Backend Unavailable at Startup](#91-auth-backend-unavailable-at-startup)
   - 9.2 [Auth Backend Crash During Operation](#92-auth-backend-crash-during-operation)
   - 9.3 [Authentication Failures](#93-authentication-failures)
   - 9.4 [Carrier Flapping During Authentication](#94-carrier-flapping-during-authentication)
   - 9.5 [Observability](#95-observability)
   - 9.6 [Operator Notifications](#96-operator-notifications)
10. [Testing Strategy](#10-testing-strategy)
11. [Implementation Phases](#11-implementation-phases)

---

## 1. Context

Ethernet is conceptually the simplest technology Nexus manages, but this simplicity is deceptive once 802.1X is in the picture. A wired interface on a modern enterprise network may require port-based authentication before any traffic is allowed — the switch will block everything except EAPOL until the supplicant authenticates. Nexus must handle both the simple case (consumer/home Ethernet with no authentication) and the enterprise case (wired 802.1X with EAP-TLS, EAP-PEAP, or similar).

The Ethernet Backend is the bridge between the Interface Monitor (which tells it when wired interfaces appear and when carrier changes) and systemd-networkd (which handles IP once the link is "ready"). For 802.1X-protected ports, "ready" means not just carrier-up but also authenticated.

For architectural context, read [nexus-architecture.md](./nexus-architecture.md) first. For how interface discovery and carrier events are produced, read [dd-001-interface-discovery.md](./dd-001-interface-discovery.md).

### 1.1 Repo Layout

The code for this component lives at:

```
crates/
  nexus-ethernet/           <- the Ethernet Backend component
    Cargo.toml
    src/
      lib.rs                <- entry point (spawn_ethernet_backend)
      backend.rs            <- EthernetBackend struct, event loop
      lifecycle.rs          <- phase 2: EthInterfaceState machine
      profile.rs            <- phase 3: per-interface profile loading
      auth/                 <- phase 4: pluggable auth backend
        mod.rs              <- WiredAuthBackend trait + AuthState
        wpa_supplicant.rs   <- WpaSupplicantWiredBackend (default)
        ead.rs              <- EadBackend (experimental; feature-gated)
        mock.rs              <- for tests
      retry.rs              <- exponential backoff for auth failures
      metrics.rs            <- metric declarations
      config.rs             <- configuration parsing (§8)
    tests/
      lifecycle_tests.rs    <- state machine coverage (§10.1)
      auth_integration.rs   <- hostapd + FreeRADIUS fixture (§10.2)

crates/
  nexus-auth-eap/           <- shared EAP config types across Wi-Fi and wired
    src/
      config.rs             <- Dot1xEapConfig (also used by DD-003)
```

The `WiredAuthBackend` trait and its concrete implementations live inside `nexus-ethernet` because they are specific to wired 802.1X. Reusable EAP configuration types (`Dot1xEapConfig`) live in `nexus-auth-eap` so the Wi-Fi Backend can use the same types for WPA2/WPA3-Enterprise without a dependency on the Ethernet crate.

---

## 2. Responsibilities

The Ethernet Backend is responsible for:

1. Tracking the lifecycle of every Ethernet interface discovered by the Interface Monitor.
2. Loading the per-interface Ethernet profile from the Profile Store.
3. Watching carrier state and reacting to link-up / link-down events.
4. If 802.1X is configured for an interface, driving the pluggable auth backend to authenticate the port when carrier comes up.
5. Signaling `LinkReady` to the core (which triggers systemd-networkd to bring up IP) once the interface is usable — i.e., carrier is up and 802.1X authentication (if configured) has succeeded.
6. Signaling `LinkLost` when carrier drops or authentication fails/expires.
7. Cleaning up auth backend state when interfaces are removed.

The Ethernet Backend is explicitly **not** responsible for:

- Carrier detection itself — that is the Interface Monitor's job.
- IP configuration — systemd-networkd handles that.
- Implementing EAPOL or EAP methods — the auth backend (wpa_supplicant or ead) does that.

---

## 3. Interface Lifecycle

### 3.1 States

```rust
enum EthInterfaceState {
    /// Interface just discovered, profile not yet loaded.
    Registered,

    /// Profile loaded, waiting for carrier.
    WaitingForCarrier,

    /// Carrier up, 802.1X not configured — signal LinkReady immediately.
    LinkReady,

    /// Carrier up, 802.1X configured, authentication in progress.
    Authenticating,

    /// Carrier up and authenticated.
    Authenticated,

    /// Carrier up but authentication failed. Waiting to retry.
    AuthFailed { retry_after: Instant, attempts: u32 },

    /// Interface removed.
    Gone,
}
```

### 3.2 State Transitions

```
                  InterfaceDiscovered
                  ─────────────────►
  [initial]                          Registered
                                         │
                                         │ profile loaded
                                         ▼
                                 WaitingForCarrier
                                         │
                          ┌──────────────┴──────────────┐
                          │ CarrierChanged(up=true)     │
                          │                              │
                  ┌───────▼──────┐              ┌───────▼────────┐
                  │  no 802.1X   │              │  802.1X        │
                  │  configured  │              │  configured    │
                  └───────┬──────┘              └───────┬────────┘
                          │                              │
                          ▼                              ▼
                     LinkReady                    Authenticating
                          │                              │
                          │                     ┌────────┴────────┐
                          │                     │                 │
                          │                     ▼ success         ▼ failure
                          │              Authenticated       AuthFailed
                          │                     │                 │
                          │                     │                 │ backoff expires
                          │                     │                 ▼
                          │                     │           Authenticating
                          │                     │             (retry)
                          │                     │
                          └────────┬────────────┘
                                   │ CarrierChanged(up=false)
                                   ▼
                           WaitingForCarrier
                                   │
                                   │ InterfaceRemoved
                                   ▼
                                 Gone
```

### 3.3 Core Lifecycle Logic

```rust
/// Per-interface state held by the Ethernet Backend.
struct EthInterfaceEntry {
    info: InterfaceInfo,
    profile: EthernetProfile,   // see DD-007 §5.2
    state: EthInterfaceState,
}

/// Handler called when a NexusEvent arrives from the bus.
/// All state transitions go through here. Methods assume
/// self.interfaces is the authoritative registry.
async fn handle_event(&mut self, event: NexusEvent) -> Result<()> {
    match event {
        NexusEvent::InterfaceDiscovered(info)
            if matches!(info.kind, InterfaceKind::Ethernet) =>
        {
            // Borrow fields out of `info` before moving it.
            let ifindex = info.ifindex;
            let had_carrier = info.carrier;
            let profile = self.profile_store
                .load_ethernet_profile(&info.ifname)
                .await?
                .unwrap_or_else(|| EthernetProfile::default_for(&info.ifname));

            self.interfaces.insert(ifindex, EthInterfaceEntry {
                info,
                profile,
                state: EthInterfaceState::WaitingForCarrier,
            });

            // If carrier is already up at discovery time (unlikely on fresh
            // boot but possible on rediscovery), kick the state machine.
            if had_carrier {
                self.on_carrier_up(ifindex).await?;
            }
        }

        NexusEvent::CarrierChanged { ifindex, up: true }
            if self.interfaces.contains_key(&ifindex) =>
        {
            self.on_carrier_up(ifindex).await?;
        }

        NexusEvent::CarrierChanged { ifindex, up: false }
            if self.interfaces.contains_key(&ifindex) =>
        {
            self.on_carrier_down(ifindex).await?;
        }

        NexusEvent::InterfaceRemoved { ifindex } => {
            if let Some(_entry) = self.interfaces.remove(&ifindex) {
                // Best-effort auth cleanup; drive-by daemon failures are ok here.
                if let Some(auth) = self.auth_backend.as_mut() {
                    let _ = auth.detach(ifindex).await;
                }
            }
        }

        NexusEvent::EthAuthStateChanged { ifindex, state } => {
            self.on_auth_state_changed(ifindex, state).await?;
        }

        _ => {}
    }
    Ok(())
}

async fn on_carrier_up(&mut self, ifindex: u32) -> Result<()> {
    // Extract the few values we need and release the registry borrow
    // before touching self.auth_backend or self.event_tx.
    enum Action {
        NoAuth,
        StartAuth { ifname: String, eap_config: Dot1xEapConfig },
    }

    let action = {
        let entry = self
            .interfaces
            .get_mut(&ifindex)
            .context("entry vanished")?;

        match entry.profile.dot1x.as_ref() {
            Some(d) if d.enabled => {
                entry.state = EthInterfaceState::Authenticating;
                Action::StartAuth {
                    ifname: entry.info.ifname.clone(),
                    eap_config: d.eap.clone(),
                }
            }
            _ => {
                entry.state = EthInterfaceState::LinkReady;
                Action::NoAuth
            }
        }
    };

    match action {
        Action::StartAuth { ifname, eap_config } => {
            let auth = self
                .auth_backend
                .as_mut()
                .context("802.1X configured but no auth backend available")?;
            auth.attach(ifindex, &ifname).await?;
            auth.authenticate(ifindex, &eap_config).await?;
            // State advances to Authenticated when the auth backend
            // emits AuthStateChanged(Authenticated); see on_auth_state_changed.
        }
        Action::NoAuth => {
            let _ = self.event_tx.send(NexusEvent::EthLinkReady { ifindex });
        }
    }
    Ok(())
}

async fn on_carrier_down(&mut self, ifindex: u32) -> Result<()> {
    let was_ready = {
        let entry = self
            .interfaces
            .get_mut(&ifindex)
            .context("entry vanished")?;
        let ready = matches!(
            entry.state,
            EthInterfaceState::LinkReady | EthInterfaceState::Authenticated
        );
        entry.state = EthInterfaceState::WaitingForCarrier;
        ready
    };

    // Carrier loss invalidates any EAPOL state — tear down cleanly.
    if let Some(auth) = self.auth_backend.as_mut() {
        let _ = auth.detach(ifindex).await;
    }

    if was_ready {
        let _ = self.event_tx.send(NexusEvent::EthLinkLost { ifindex });
    }
    Ok(())
}

async fn on_auth_state_changed(
    &mut self,
    ifindex: u32,
    state: AuthState,
) -> Result<()> {
    let Some(entry) = self.interfaces.get_mut(&ifindex) else {
        return Ok(());
    };

    match state {
        AuthState::Authenticated => {
            entry.state = EthInterfaceState::Authenticated;
            let _ = self.event_tx.send(NexusEvent::EthLinkReady { ifindex });
        }
        AuthState::Failed { reason } => {
            let attempts = match entry.state {
                EthInterfaceState::AuthFailed { attempts, .. } => attempts + 1,
                _ => 1,
            };
            let retry_after = self.retry.compute_next_attempt(reason, attempts);
            entry.state = EthInterfaceState::AuthFailed { retry_after, attempts };
            // A retry timer elsewhere will pick this up and call authenticate()
            // again, subject to the fail-fast rules in §9.3.
        }
        AuthState::Authenticating | AuthState::Idle => {
            // Intermediate; no lifecycle transition here.
        }
    }
    Ok(())
}
```

**On event emission.** The backend holds `event_tx: broadcast::Sender<NexusEvent>` (wired at construction time; see [the trait note in §5.1](#51-trait-definition)). All outbound signals go through this sender. Receivers elsewhere in Nexus (the core state machine, D-Bus service layer) subscribe through their own `Receiver` handles. `send()` is best-effort — returning `Err(SendError)` when there are no receivers is fine and not propagated.

---

## 4. IEEE 802.1X Authentication

### 4.1 Protocol Overview

IEEE 802.1X is a port-based network access control standard. In a wired deployment, three roles are involved:

- **Supplicant** — The client device seeking access (Nexus via its auth backend).
- **Authenticator** — The switch port the client is connected to.
- **Authentication Server** — Typically a RADIUS server behind the switch.

The supplicant and authenticator communicate using **EAPOL** (EAP Over LAN), which rides directly on Ethernet at ethertype `0x888e` (`ETH_P_PAE`). EAPOL encapsulates **EAP** (Extensible Authentication Protocol) frames that carry the actual authentication exchange. Inside EAP, any number of authentication methods can be negotiated:

- **EAP-TLS** — Mutual certificate authentication (most common in enterprise).
- **EAP-PEAP** — TLS tunnel protecting an inner password-based method (MSCHAPv2 typical).
- **EAP-TTLS** — Similar to PEAP with different tunnel semantics.
- **EAP-MD5** — Legacy password hashing, deprecated.

Until authentication succeeds, the switch port is in "unauthorized" state and blocks all traffic except EAPOL frames. After success, the port transitions to "authorized" and normal traffic is allowed. Periodic re-authentication may be required by the authenticator.

### 4.2 Nexus's Role

Nexus does not implement EAPOL or EAP itself. It delegates this to one of two auth backends:

- **wpa_supplicant** — The de-facto 802.1X supplicant on Linux. Mature, universally available, uses OpenSSL (or other TLS libraries at build time) for EAP-TLS/PEAP/TTLS. Handles both wired and wireless 802.1X on the same daemon.
- **ead** (Ethernet Authentication Daemon) — Part of the iwd project. Lighter, uses the kernel crypto API instead of OpenSSL, no wireless dependency. Less mature than wpa_supplicant for wired use.

The Ethernet Backend drives whichever auth backend is configured. Both backends expose a D-Bus interface that follows similar patterns but with different object hierarchies. The pluggable abstraction (Section 5) hides these differences.

### 4.3 Authentication Flow (Wired EAP-TLS Example)

```
Nexus              Auth Backend              Switch              RADIUS Server
  │                      │                     │                        │
  │ attach(eth0)         │                     │                        │
  ├─────────────────────►│                     │                        │
  │                      │                     │                        │
  │ authenticate(config) │                     │                        │
  ├─────────────────────►│                     │                        │
  │                      │                     │                        │
  │                      │  EAPOL-Start        │                        │
  │                      ├────────────────────►│                        │
  │                      │                     │                        │
  │                      │  EAP-Request/Id     │                        │
  │                      │◄────────────────────┤                        │
  │                      │                     │                        │
  │                      │  EAP-Response/Id    │                        │
  │                      ├────────────────────►│ Access-Request         │
  │                      │                     ├───────────────────────►│
  │                      │                     │                        │
  │                      │  EAP-Request/TLS    │ Access-Challenge       │
  │                      │◄────────────────────┤◄───────────────────────┤
  │                      │                     │                        │
  │                   (TLS handshake, client cert, server cert, ...)    │
  │                      │                     │                        │
  │                      │  EAP-Success        │ Access-Accept          │
  │                      │◄────────────────────┤◄───────────────────────┤
  │                      │                     │ (port: AUTHORIZED)     │
  │ state=Authenticated  │                     │                        │
  │◄─────────────────────┤                     │                        │
  │                      │                     │                        │
  │ emit EthLinkReady    │                     │                        │
  ▼                      │                     │                        │
```

On success, the Ethernet Backend transitions the interface to `Authenticated` and emits `EthLinkReady`, which triggers systemd-networkd to begin IP configuration (DHCP or static).

---

## 5. Pluggable Auth Backend

### 5.1 Trait Definition

```rust
/// A wired 802.1X authentication backend.
///
/// Implementations drive an external daemon (wpa_supplicant or ead) over D-Bus
/// to perform the EAPOL/EAP exchange on a given Ethernet interface.
///
/// Progress is reported asynchronously via the Nexus event bus: every concrete
/// impl takes a `broadcast::Sender<NexusEvent>` in its `new(...)` constructor
/// and emits `NexusEvent::EthAuthStateChanged { ifindex, state }` whenever the
/// per-interface `AuthState` changes. The Ethernet Backend consumes these
/// events through its normal event loop (see §3.3) and drives the lifecycle
/// accordingly — it does NOT poll `state()` in the steady state.
///
/// Construction convention (not part of the trait because constructors can't
/// be members of object-safe traits):
/// ```ignore
/// impl WpaSupplicantWiredBackend {
///     pub async fn new(
///         conn: zbus::Connection,
///         event_tx: broadcast::Sender<NexusEvent>,
///     ) -> Result<Self> { ... }
/// }
/// ```
#[async_trait]
pub trait WiredAuthBackend: Send + Sync {
    /// Register the interface with the auth daemon. This creates the
    /// daemon-side interface object but does not start authentication.
    async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()>;

    /// Start authentication on the interface using the provided EAP config.
    ///
    /// Must be called after `attach`. Safe to call multiple times for the
    /// same interface — implementations must clean up any prior network
    /// configuration on the supplicant side before starting a new one, so
    /// that retries don't accumulate network entries. Returns immediately;
    /// progress is reported via `EthAuthStateChanged` events on the bus.
    async fn authenticate(
        &mut self,
        ifindex: u32,
        config: &Dot1xEapConfig,
    ) -> Result<()>;

    /// Stop authentication and unregister the interface from the auth daemon.
    /// Best-effort; errors are logged but not propagated.
    async fn detach(&mut self, ifindex: u32) -> Result<()>;

    /// Query the current authentication state for an interface.
    /// For diagnostics / D-Bus property reads only; not called in the steady-state
    /// event loop.
    async fn state(&self, ifindex: u32) -> Result<AuthState>;

    /// Backend identifier for logging and metrics.
    fn name(&self) -> &'static str;
}

#[derive(Debug, Clone)]
pub enum AuthState {
    /// Not yet authenticating (pre-attach, or after detach). Emitted rarely;
    /// the Ethernet Backend's state machine uses its own EthInterfaceState
    /// to track "waiting for auth to start."
    Idle,
    Authenticating,
    Authenticated,
    Failed { reason: AuthFailureReason },
}

#[derive(Debug, Clone)]
pub enum AuthFailureReason {
    BadCredentials,
    ServerUnreachable,
    CertificateRejected,
    Timeout,
    Other(String),
}
```

**Note on zbus pseudocode.** The method implementations below use `zbus::Value::from(&str)` and similar patterns for brevity. `zbus::Value<'_>` borrows its input; when building a `HashMap<String, Value<'_>>` that outlives the function body, implementations must own the data — either by using `Value::from(String)` with owned strings, or by explicitly managing lifetimes. The pseudocode here omits these details for readability; see the actual crate implementations for the exact pattern.

### 5.2 Configuration Struct

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct Dot1xEapConfig {
    /// EAP method: "TLS", "PEAP", "TTLS", "MSCHAPV2", ...
    pub eap: String,

    /// User identity (anonymous or real, depending on method).
    pub identity: String,

    /// Anonymous outer identity for PEAP/TTLS.
    pub anonymous_identity: Option<String>,

    /// Password, if using a password-based method.
    /// Stored encrypted in the Profile Store; decrypted at load time.
    pub password: Option<SecretString>,

    /// CA certificate (validates the authentication server).
    pub ca_cert: Option<PathBuf>,

    /// Client certificate (for EAP-TLS).
    pub client_cert: Option<PathBuf>,

    /// Client private key (for EAP-TLS).
    pub private_key: Option<PathBuf>,

    /// Private key passphrase, if the key is encrypted.
    pub private_key_passwd: Option<SecretString>,

    /// Inner (phase-2) authentication for PEAP/TTLS. E.g., "auth=MSCHAPV2".
    pub phase2: Option<String>,

    /// Domain suffix to match against the server certificate's SAN.
    /// Strongly recommended to prevent rogue RADIUS impersonation.
    pub domain_suffix_match: Option<String>,
}
```

### 5.3 Backend Selection

The auth backend is selected at startup from configuration:

```toml
# /etc/nexus/nexus.toml
[ethernet.auth]
backend = "wpa_supplicant"  # or "ead", or "none"
```

When `backend = "none"`, Nexus treats any profile with 802.1X configured as an error at load time. When set to `wpa_supplicant` or `ead`, Nexus instantiates the corresponding backend at startup and fails fast if the daemon is not available.

At build time, Nexus uses Cargo features to control which backends are compiled in:

```toml
[features]
default = ["auth-wpa_supplicant"]
auth-wpa_supplicant = ["zbus"]
auth-ead = ["zbus"]
```

Embedded integrators can build with only `auth-ead` enabled if they want to avoid the wpa_supplicant dependency entirely.

### 5.4 Default Choice

**Nexus defaults to wpa_supplicant for wired 802.1X.**

Rationale:

- Production-grade maturity — wired 802.1X in wpa_supplicant has been deployed in enterprise environments for over a decade.
- Wide EAP method coverage (TLS, PEAP, TTLS, FAST, SIM/AKA, etc.).
- Unified codebase with wireless 802.1X if Nexus is already using wpa_supplicant for Wi-Fi (which is the most common case).

ead is supported as an alternative but is considered experimental in the current release. Integrators who prefer ead should validate their specific EAP method against it before deploying.

---

## 6. wpa_supplicant Auth Backend

### 6.1 D-Bus Service

wpa_supplicant exposes `fi.w1.wpa_supplicant1` on the system bus. The same service handles both wired and wireless interfaces — the driver parameter distinguishes them:

- For wireless interfaces: `Driver: "nl80211"` (kernel Wi-Fi stack).
- For wired interfaces: `Driver: "wired"` (raw EAPOL over the Ethernet interface).

### 6.2 Attach

```rust
async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()> {
    let root = WpaSupplicant1Proxy::new(&self.conn).await?;

    let path = root.create_interface(HashMap::from([
        ("Ifname".into(), Value::from(ifname)),
        ("Driver".into(), Value::from("wired")),
    ])).await;

    let interface_path = match path {
        Ok(p) => p,
        Err(e) if is_interface_exists_error(&e) => {
            // Interface exists — either a prior Nexus instance registered it
            // or another caller did. Reuse the interface, but cancel any
            // in-flight operation it may have been in the middle of.
            let existing = root.get_interface(ifname).await?;
            let iface = Interface1Proxy::builder(&self.conn)
                .path(existing.clone())?
                .build().await?;
            // Disconnect any previous state before we take over.
            let _ = iface.disconnect().await;
            existing
        }
        Err(e) => return Err(e.into()),
    };

    // Subscribe to PropertiesChanged for state tracking
    let iface_proxy = Interface1Proxy::builder(&self.conn)
        .path(interface_path.clone())?
        .build().await?;

    let state_stream = iface_proxy.receive_properties_changed().await?;
    // Spawn a task that watches state changes; see spawn_state_watcher
    // below. The returned JoinHandle is stored so detach can abort it.
    let watcher = self.spawn_state_watcher(ifindex, state_stream);

    self.registered.insert(ifindex, RegisteredInterface {
        ifname: ifname.to_string(),
        dbus_path: interface_path,
        active_network: None,
        watcher,
    });
    Ok(())
}

/// Spawn a background task that translates supplicant PropertiesChanged
/// events into NexusEvent::EthAuthStateChanged. Returns a JoinHandle the
/// caller aborts on detach to release D-Bus subscriptions.
fn spawn_state_watcher(
    &self,
    ifindex: u32,
    mut stream: PropertiesChangedStream<'static>,
) -> tokio::task::JoinHandle<()> {
    let event_tx = self.event_tx.clone();
    tokio::spawn(async move {
        while let Some(signal) = stream.next().await {
            let Ok(args) = signal.args() else { continue };
            let Some(state_val) = args.changed_properties().get("State") else { continue };
            let Ok(state_str) = state_val.downcast_ref::<str>() else { continue };
            let new_state = translate_wpa_state(state_str);  // see §6.4 table
            let _ = event_tx.send(NexusEvent::EthAuthStateChanged {
                ifindex,
                state: new_state,
            });
        }
        // Stream ended (daemon went away) — emit Idle so the lifecycle
        // layer notices. The Ethernet Backend's on_auth_state_changed
        // handles this gracefully.
        let _ = event_tx.send(NexusEvent::EthAuthStateChanged {
            ifindex,
            state: AuthState::Idle,
        });
    })
}
```

Where `RegisteredInterface` tracks the active network handle and the watcher task:

```rust
struct RegisteredInterface {
    ifname: String,
    dbus_path: OwnedObjectPath,
    active_network: Option<OwnedObjectPath>,  // supplicant-side network object
    watcher: tokio::task::JoinHandle<()>,
}
```

### 6.3 Authenticate

```rust
async fn authenticate(&mut self, ifindex: u32, config: &Dot1xEapConfig) -> Result<()> {
    let entry = self.registered.get_mut(&ifindex).context("not attached")?;
    let iface = Interface1Proxy::builder(&self.conn)
        .path(&entry.dbus_path)?
        .build().await?;

    // If a previous authentication left a network block installed on the
    // supplicant side, remove it before installing the new one. Without
    // this, retries accumulate network entries and eventually break the
    // supplicant's selection logic.
    if let Some(old_net) = entry.active_network.take() {
        let _ = iface.remove_network(&old_net).await;  // best-effort
    }

    // Build a network block equivalent to wpa_supplicant.conf:
    //   key_mgmt=IEEE8021X
    //   eap=TLS
    //   identity="user@domain"
    //   ca_cert="/path/to/ca.pem"
    //   client_cert="/path/to/client.pem"
    //   private_key="/path/to/client.key"
    //   private_key_passwd="..."
    //   eapol_flags=0        (important for wired; see note below)

    let mut args: HashMap<String, Value<'static>> = HashMap::new();
    args.insert("key_mgmt".into(), Value::from("IEEE8021X".to_string()));
    args.insert("eap".into(), Value::from(config.eap.clone()));
    args.insert("identity".into(), Value::from(config.identity.clone()));
    if let Some(anon) = &config.anonymous_identity {
        args.insert("anonymous_identity".into(), Value::from(anon.clone()));
    }
    if let Some(ca) = &config.ca_cert {
        args.insert("ca_cert".into(), Value::from(ca.to_string_lossy().into_owned()));
    }
    if let Some(cert) = &config.client_cert {
        args.insert("client_cert".into(), Value::from(cert.to_string_lossy().into_owned()));
    }
    if let Some(key) = &config.private_key {
        args.insert("private_key".into(), Value::from(key.to_string_lossy().into_owned()));
    }
    if let Some(passwd) = &config.private_key_passwd {
        args.insert("private_key_passwd".into(), Value::from(passwd.expose_secret().to_string()));
    }
    if let Some(pw) = &config.password {
        args.insert("password".into(), Value::from(pw.expose_secret().to_string()));
    }
    if let Some(phase2) = &config.phase2 {
        args.insert("phase2".into(), Value::from(phase2.clone()));
    }
    if let Some(domain) = &config.domain_suffix_match {
        args.insert("domain_suffix_match".into(), Value::from(domain.clone()));
    }
    // Wired-specific: disable WPA-style EAPOL key exchange
    args.insert("eapol_flags".into(), Value::from(0u32));

    let net_path = iface.add_network(args).await?;
    iface.select_network(&net_path).await?;
    entry.active_network = Some(net_path);
    Ok(())
}
```

**Note on `eapol_flags`.** In wireless 802.1X, after EAP-Success the supplicant and authenticator derive PMK/PTK and perform a 4-way handshake. In wired 802.1X, there is no such handshake — EAP-Success is sufficient. Setting `eapol_flags=0` disables the WPA key derivation step that would otherwise hang indefinitely on a wired interface.

### 6.4 State Tracking

wpa_supplicant's `State` property transitions through:

```
disconnected → scanning → associating → associated → authenticating
             → 4way_handshake → group_handshake → completed
             (failure paths: disconnected with disconnect_reason)
```

For wired 802.1X, the `scanning` / `associating` / `associated` phases are trivial — there's no scan or association on wired. The meaningful states are:

| wpa_supplicant `State` | Nexus `AuthState` |
|---|---|
| `authenticating` | `Authenticating` |
| `completed` | `Authenticated` |
| `disconnected` (prior state was `completed` or `authenticating`) | see disconnect handling below |
| all other transitional states | no emit |

**Disconnect handling — distinguishing failure from teardown.** A transition to `disconnected` does NOT unconditionally mean authentication failed. It can also mean:

- Nexus called `detach()` (e.g., carrier went down, interface was removed). The previous `detach()` call is recent and recorded in backend state; correlate and **suppress** the event — Nexus is already driving the teardown.
- wpa_supplicant was asked to disconnect by another control-socket client (unusual but possible). Emit as `Failed { Other("external_disconnect") }`.
- The authenticator deauthenticated the client. Read the `DisconnectReason` property (signed integer; negative = locally initiated, positive = 802.11 reason code). Map to `AuthFailureReason`:

  | DisconnectReason | Interpretation | Map to |
  |---|---|---|
  | `-3` | Local request (Nexus initiated) | suppress |
  | `0`, `1` | Unspecified / AP-initiated | `Other("unspecified")` |
  | `15` | 4-way handshake timeout (should not apply to wired) | `Timeout` |
  | `23` | 802.1X authentication failed | `BadCredentials` |
  | Any EAP-Failure with `CertificateRejected` flag in extended ACK | `CertificateRejected` |

The backend watches `PropertiesChanged` signals on the interface object and emits `EthAuthStateChanged` events on the Nexus event bus only for meaningful transitions — noise-free so the Ethernet Backend's `on_auth_state_changed` handler can make clean decisions.

### 6.5 Detach

```rust
async fn detach(&mut self, ifindex: u32) -> Result<()> {
    if let Some(entry) = self.registered.remove(&ifindex) {
        // Abort the PropertiesChanged watcher. It emits one final
        // AuthState::Idle as it unwinds (see spawn_state_watcher).
        entry.watcher.abort();

        let root = WpaSupplicant1Proxy::new(&self.conn).await?;
        let _ = root.remove_interface(&entry.dbus_path).await;
        // RemoveInterface is best-effort — if wpa_supplicant already
        // removed it (e.g., daemon restart), the error is benign.
    }
    Ok(())
}
```

---

## 7. ead Auth Backend

### 7.1 D-Bus Service

ead exposes its D-Bus API under the `net.connman.ead` service name (inherited from the ConnMan namespace, despite having no dependency on ConnMan). The object hierarchy is simpler than wpa_supplicant's:

```
net.connman.ead
  /
    net.connman.ead.Manager
      → GetAdapters() → a(oa{sv})
  /{adapter_path}
    net.connman.ead.Adapter
      → Connect()
      → Disconnect()
      → properties: Name, Authenticated, ...
```

Each "adapter" in ead terminology corresponds to one Ethernet interface.

### 7.2 Attach and Authenticate

ead does not have a direct `CreateInterface()` equivalent. It auto-discovers Ethernet interfaces and exposes them as adapter objects. The Nexus backend's job is to:

1. Discover the adapter path for the given ifindex via ead's `GetAdapters()`.
2. Configure credentials for the adapter (via profile files or D-Bus).
3. Call `Connect()` to trigger authentication.

```rust
async fn authenticate(&mut self, ifindex: u32, config: &Dot1xEapConfig) -> Result<()> {
    // Write credentials to /var/lib/ead/{ifname}.8021x using the same
    // keyfile format iwd uses for its Wi-Fi profiles
    self.write_profile(ifindex, config).await?;

    let adapter_path = self.find_adapter(ifindex).await?;
    let adapter = EadAdapter1Proxy::builder(&self.conn)
        .path(&adapter_path)?
        .build().await?;

    adapter.connect().await?;
    Ok(())
}
```

### 7.3 Limitations

At the time of this design:

- **EAP method coverage is narrower** than wpa_supplicant. EAP-TLS and EAP-PEAP work; less common methods may not be supported.
- **Mature distro integration is spotty.** Some distros don't ship a systemd service file for ead, requiring manual enablement.
- **Documentation is minimal.** The upstream docs consist primarily of the man page and source code.

For these reasons, Nexus's ead backend is labeled experimental and not recommended as the default. Integrators choosing ead should validate their specific EAP method and credential format before deploying.

---

## 8. Configuration

### 8.1 Global Configuration

In `/etc/nexus/nexus.toml`:

```toml
[ethernet]
# Auth backend. "wpa_supplicant" | "ead" | "none"
auth_backend = "wpa_supplicant"

# Authentication failure retry policy
auth_retry_initial_ms = 1000
auth_retry_max_ms = 60000
auth_retry_multiplier = 2.0
auth_max_attempts = 0  # 0 = unlimited
```

### 8.2 Per-Interface Profile

In `/var/lib/nexus/ethernet/{ifname}.toml`:

```toml
# Minimal profile — no authentication, just tracking
schema_version = 1
id = "01HPQY8S2N0Z8K9M7V3Y2F4T5W6"   # ULID; see DD-007

[interface]
name = "eth0"
auto_connect = true

# 802.1X with EAP-TLS
schema_version = 1
id = "01HPQY8S3A1B8K9M7V3Y2F4T5W7"

[interface]
name = "eth1"
auto_connect = true

[interface.dot1x]
enabled = true

[interface.dot1x.eap]
eap = "TLS"
identity = "device-0001@corp.example.com"
ca_cert = "/etc/nexus/certs/corp-ca.pem"
client_cert = "/etc/nexus/certs/device-0001.pem"
private_key = "/etc/nexus/certs/device-0001.key"
# private_key_passwd stored encrypted per DD-007 §4.4 wire format:
# private_key_passwd = { enc = "v1", nonce = "...", ct = "..." }
domain_suffix_match = "corp.example.com"

# 802.1X with EAP-PEAP + MSCHAPv2
schema_version = 1
id = "01HPQY8S4B2C8K9M7V3Y2F4T5W8"

[interface]
name = "eth2"
auto_connect = true

[interface.dot1x]
enabled = true

[interface.dot1x.eap]
eap = "PEAP"
identity = "user@corp.example.com"
anonymous_identity = "anonymous@corp.example.com"
# password stored encrypted per DD-007 §4.4
password = { enc = "v1", nonce = "...", ct = "..." }
ca_cert = "/etc/nexus/certs/corp-ca.pem"
phase2 = "auth=MSCHAPV2"
domain_suffix_match = "corp.example.com"
```

See [DD-007: Profile Store](./dd-007-profile-store.md) §3.3 for the canonical profile file shape, §4.4 for the encrypted-field wire format, and §8 for `schema_version` semantics.

---

## 9. Error Handling

### 9.1 Auth Backend Unavailable at Startup

If `auth_backend = "wpa_supplicant"` but the `fi.w1.wpa_supplicant1` D-Bus name is not present at Nexus startup:

- Log a warning.
- Set up a `NameOwnerChanged` watch on the bus name.
- Accept incoming Ethernet interfaces into the registry.
- For interfaces with 802.1X configured, transition to `AuthFailed` immediately with reason `ServerUnreachable`. For interfaces without 802.1X, proceed to `LinkReady` normally.
- When `NameOwnerChanged` fires, retry authentication on all pending interfaces.

### 9.2 Auth Backend Crash During Operation

Detected via `NameOwnerChanged` transitioning from "owned" to "not owned":

- All interfaces in `Authenticating` or `Authenticated` state are transitioned to `AuthFailed`.
- `EthLinkLost` is emitted for any interface that was in `Authenticated`.
- When the name reappears, each affected interface's auth retry timer kicks in and `authenticate()` is called again.

### 9.3 Authentication Failures

| Failure | Reason | Retry? |
|---|---|---|
| Bad credentials (EAP-Failure from server) | `BadCredentials` | No — fail fast, mark profile invalid |
| Server unreachable (EAP timeout) | `ServerUnreachable` | Yes — with exponential backoff |
| Certificate validation failed | `CertificateRejected` | No — fail fast |
| Auth daemon internal timeout | `Timeout` | Yes — with exponential backoff |
| Unknown | `Other(String)` | Yes — conservative |

"No retry" cases should surface to the operator via D-Bus signal so the profile can be fixed.

### 9.4 Carrier Flapping During Authentication

If carrier drops while `Authenticating`:

- Cancel the authentication by calling `auth.detach(ifindex)`.
- Transition to `WaitingForCarrier`.
- When carrier returns, restart authentication from scratch (not resume — EAPOL state doesn't survive link-down).

### 9.5 Observability

The Ethernet Backend exposes the following metrics:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `nexus_eth_interfaces_managed` | gauge | `state` | Interfaces by current state (`waiting_carrier`/`link_ready`/`authenticating`/`authenticated`/`auth_failed`) |
| `nexus_eth_link_ready_total` | counter | `ifname` | `EthLinkReady` emissions |
| `nexus_eth_link_lost_total` | counter | `ifname`, `reason` (`carrier_down`/`auth_failure`/`removed`) | `EthLinkLost` emissions |
| `nexus_eth_auth_attempts_total` | counter | `ifname`, `outcome` (`success`/`bad_credentials`/`server_unreachable`/`cert_rejected`/`timeout`/`other`) | Authentication attempt outcomes |
| `nexus_eth_auth_duration_seconds` | histogram | `ifname` | Time from `Authenticating` entry to final `Authenticated` or `AuthFailed` |
| `nexus_eth_auth_retries_total` | counter | `ifname` | Retry attempts after a retriable failure |
| `nexus_eth_auth_backend_available` | gauge | `backend` (`wpa_supplicant`/`ead`) | 1 if the D-Bus name is present, 0 otherwise |

### 9.6 Operator Notifications

For fail-fast cases (bad credentials, rejected certificate), the Ethernet Backend emits a D-Bus signal on the interface's D-Bus object so operator UIs can surface the issue. The signal name and payload are specified in [DD-006: D-Bus API](./dd-006-dbus-api.md) §9.

---

## 10. Testing Strategy

### 10.1 Unit Tests

- State machine transitions: feed synthetic events into the lifecycle and assert correct state progression.
- Configuration parsing: valid and invalid profile TOML.
- Retry timer logic: exponential backoff math.

### 10.2 Integration Tests

- **With wpa_supplicant:** Use a local FreeRADIUS instance and a veth pair with one end attached to a `hostapd`-wired-authenticator simulator. Verify EAP-TLS and EAP-PEAP flows end to end.
- **With ead:** Same fixture, swap the backend at runtime, re-run the same test matrix.
- **Daemon crash recovery:** Kill wpa_supplicant mid-authentication, verify Nexus transitions to `AuthFailed` and recovers when the daemon restarts.

### 10.3 Manual Validation Matrix

Before shipping, validate against at least the following real infrastructure:

- Cisco Catalyst switch with RADIUS (EAP-TLS, EAP-PEAP-MSCHAPv2).
- Aruba switch with RADIUS.
- Ubiquiti UniFi switch (where supported) with RADIUS.
- FreeRADIUS and Windows Server NPS backends.

---

## 11. Implementation Phases

Suggested build order. Phases align with the repo layout in §1.1. DD-001 must be at least at phase 5 (event emission) before phase 1 here can start — the Ethernet Backend consumes `InterfaceDiscovered` and `CarrierChanged` events.

### Phase 1 — Skeleton and Lifecycle State Machine

`crates/nexus-ethernet/src/lifecycle.rs` and `src/backend.rs`.

- `EthInterfaceState` enum and transition table (§3).
- `EthernetBackend` struct with a `tokio::select!` loop that consumes `NexusEvent` from the bus.
- Handle `InterfaceDiscovered`, `CarrierChanged`, `InterfaceRemoved` with no auth — just the no-802.1X path.
- Emit `EthLinkReady` / `EthLinkLost` through the bus.

**Exit criterion:** Unplugging/plugging a cable produces correct state transitions on a test system with no 802.1X configured. Unit tests for the state machine pass.

### Phase 2 — Profile Loading

`src/profile.rs`. Parse per-interface TOML profiles per §8. No encryption yet — accept plaintext credentials for now (DD-007 will add encrypted storage).

**Exit criterion:** A profile with 802.1X configured is parsed correctly and its `Dot1xEapConfig` is available to the backend when the interface comes up.

### Phase 3 — WiredAuthBackend Trait + Mock

`src/auth/mod.rs` (trait) + `src/auth/mock.rs` (test implementation).

- Define the trait exactly as in §5.1.
- Implement a mock that can be programmed to report "success after N seconds" or "fail with reason X".
- Wire the backend to route auth-required interfaces through the trait.

**Exit criterion:** With the mock backend, the full `Authenticating → Authenticated` and `Authenticating → AuthFailed → retry` paths work. Unit tests cover all failure modes in §9.3.

### Phase 4 — wpa_supplicant Auth Backend

`src/auth/wpa_supplicant.rs`. The default production backend (§6).

- Attach via `CreateInterface` with `Driver: "wired"`.
- Build network args with `key_mgmt=IEEE8021X`, EAP-specific attributes, and critically `eapol_flags=0` (§6.3 note).
- Subscribe to `PropertiesChanged` and translate wpa_supplicant's `State` values into `AuthState`.
- Detach via `RemoveInterface`.

**Exit criterion:** On a test fixture with a hostapd-wired authenticator and FreeRADIUS, Nexus authenticates via EAP-TLS and EAP-PEAP-MSCHAPv2 end-to-end.

### Phase 5 — Retry Policy

`src/retry.rs`. Exponential backoff per §8 config.

- Per-interface retry state (attempts, next-retry-at).
- Distinguish "fail fast" reasons (`BadCredentials`, `CertificateRejected`) from "retry" reasons (`ServerUnreachable`, `Timeout`).
- Clear retry state on successful authentication or profile change.

**Exit criterion:** Fault-injection tests confirm retry timing and that fast-fail reasons don't loop.

### Phase 6 — Auth Backend Crash Recovery

Handle `NameOwnerChanged` on `fi.w1.wpa_supplicant1` per §9.1–9.2.

- On daemon disappearance: transition all `Authenticating` / `Authenticated` interfaces to `AuthFailed { ServerUnreachable }`.
- On daemon reappearance: re-attach and retry.

**Exit criterion:** Killing wpa_supplicant mid-authentication causes clean transition and recovery after systemd restarts it.

### Phase 7 — ead Auth Backend (Optional, Feature-Gated)

`src/auth/ead.rs`. Implement behind a `auth-ead` Cargo feature. Per §7.

**Exit criterion:** With the feature enabled and ead configured, EAP-TLS authentication works against the same fixture as phase 4.

### Phase 8 — Metrics & Integration Tests

- Declare metrics parallel to DD-001 §9.5 (per-interface auth state gauge, retry counter, etc.).
- Full integration test matrix from §10.2.

**Exit criterion:** CI green. Backend ready for production use with wpa_supplicant; ead marked experimental.

### Parallel work

Phases 1, 2, 3 can proceed in parallel after the shared types in `nexus-core` and `nexus-auth-eap` are stable. Phase 4 onward is sequential.

---

## Related Documents

- [Nexus Architecture](./nexus-architecture.md) — Parent architecture document
- [DD-001: Interface Discovery](./dd-001-interface-discovery.md) — How interfaces and carrier events are produced
- [DD-003: Wi-Fi Backend](./dd-003-wifi-backend.md) — Shares the supplicant abstraction pattern and `Dot1xEapConfig`
- [DD-007: Profile Store](./dd-007-profile-store.md) — How 802.1X credentials are encrypted at rest
