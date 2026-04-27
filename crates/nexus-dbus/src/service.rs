//! `DbusService` — registers every D-Bus object, runs the event
//! loop that keeps the shared [`State`] in sync with the
//! `NexusEvent` bus, and processes [`ServiceCommand`]s issued by
//! mutating-method handlers (profile add/remove → register /
//! unregister object on the live `ObjectServer`).
//!
//! Per-property `PropertiesChanged` signal emission is left to
//! phase 8 (DD-006 §12); for phase 1-6, clients read properties
//! on demand or subscribe to `ObjectManager`.

use std::sync::Arc;

use nexus_core::{InterfaceKind, NexusEvent};
use nexus_profile_store::ProfileStore;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use ulid::Ulid;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::authz::AuthChecker;
use crate::backend_ops::BackendOps;
use crate::interfaces::{
    BluetoothIface, EthernetIface, GnssIface, InterfaceIface, WifiIface, iface_names,
};
use crate::manager::Manager;
use crate::object_manager::{self, IfaceMap, ObjectManager};
use crate::paths::{
    MANAGER_PATH, ethernet_profile_path, interface_path, scan_result_path, wifi_profile_path,
};
use crate::profiles::{
    EthernetProfileIface, ProfileIface, ProfileKind, WifiProfileIface,
    iface_names as prof_iface_names,
};
use crate::properties::COALESCE_WINDOW;
use crate::rate_limit::{RateLimiter, RateLimits};
use crate::scan_results::ScanResultIface;
use crate::services::{EnabledFeatures, Services};
use crate::state::{InterfaceKindData, InterfaceState, State};

/// Configuration for [`spawn_dbus_service`].
#[derive(Clone)]
pub struct DbusConfig {
    /// Bus name to request. Defaults to `"fi.nexus1"`.
    pub bus_name: String,
    /// `true` → session bus; `false` → system bus. Ignored when
    /// [`Self::address`] is `Some`.
    pub use_session_bus: bool,
    /// Explicit D-Bus address. When set, overrides
    /// `use_session_bus`. Useful for tests with a private
    /// `dbus-daemon`.
    pub address: Option<String>,
    /// Daemon version for the `Version` property.
    pub version: String,
    /// Authorization checker — see [`crate::authz`].
    pub auth: Arc<dyn AuthChecker>,
    /// Backend command router — see [`crate::backend_ops`].
    pub ops: Arc<dyn BackendOps>,
    /// Per-class rate limits.
    pub rate_limits: RateLimits,
    /// Per-backend enable/disable flags (DD-006 §11.1
    /// `FeatureDisabled`). Mutating methods on a disabled feature
    /// return `fi.nexus.Error.FeatureDisabled`.
    pub enabled_features: EnabledFeatures,
    /// DD-006 §6.2 `fi.nexus.Ethernet.AuthBackend`. The string the
    /// daemon stamps onto every Ethernet interface's cache; one of
    /// `"wpa_supplicant"`, `"ead"`, `"none"`.
    pub ethernet_auth_backend: String,
    /// DD-006 §6.3 `fi.nexus.Wifi.Supplicant`. The string the daemon
    /// stamps onto every Wi-Fi interface's cache; one of
    /// `"wpa_supplicant"`, `"iwd"`. `"mock"` is also accepted for
    /// test harnesses but never appears in production deployments.
    pub wifi_supplicant: String,
    /// DD-006 §6.3 `fi.nexus.Wifi.RoamingMode`. Initial value
    /// stamped onto every Wi-Fi interface's cache; one of `"off"`,
    /// `"supplicant"`, `"nexus"`. Operators can change it at
    /// runtime via the writable property.
    pub wifi_roaming_mode: String,
}

impl std::fmt::Debug for DbusConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbusConfig")
            .field("bus_name", &self.bus_name)
            .field("use_session_bus", &self.use_session_bus)
            .field("address", &self.address)
            .field("version", &self.version)
            .finish()
    }
}

impl Default for DbusConfig {
    fn default() -> Self {
        Self {
            bus_name: "fi.nexus1".to_owned(),
            use_session_bus: false,
            address: None,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            auth: crate::authz::always_allow(),
            ops: crate::backend_ops::NoopOps::arc(),
            rate_limits: RateLimits::default(),
            enabled_features: EnabledFeatures::default(),
            ethernet_auth_backend: "none".to_owned(),
            wifi_supplicant: "wpa_supplicant".to_owned(),
            wifi_roaming_mode: "supplicant".to_owned(),
        }
    }
}

/// Commands the per-object methods send back to the service event
/// loop so it can mutate the live `ObjectServer`. Defined here so
/// `manager.rs` and `profiles/common.rs` can dispatch into the
/// shared registration helpers without re-implementing them.
#[derive(Debug)]
pub enum ServiceCommand {
    RegisterWifiProfile(Ulid),
    RegisterEthernetProfile(Ulid),
    UnregisterWifiProfile(Ulid),
    UnregisterEthernetProfile(Ulid),
    RegisterScanResult {
        ifname: String,
        bssid: nexus_core::MacAddr,
    },
    UnregisterScanResult {
        ifname: String,
        bssid: nexus_core::MacAddr,
    },
}

/// Handle returned by [`spawn_dbus_service`].
pub struct DbusServiceHandle {
    pub connection: zbus::Connection,
    pub state: Arc<RwLock<State>>,
    pub services: Arc<Services>,
    pub shutdown: CancellationToken,
    pub registry_tx: mpsc::Sender<ServiceCommand>,
    pub join: tokio::task::JoinHandle<crate::errors::Result<()>>,
}

impl DbusServiceHandle {
    pub async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.join.await;
    }
}

/// Build and register the service, then spawn the event-translation
/// task. Returns once the bus name is owned and `/fi/nexus1` is
/// live.
///
/// `event_rx` is passed in by the daemon rather than derived via
/// `event_tx.subscribe()` inside this function: that would race
/// interface_monitor's cold-boot dump, which happens earlier in the
/// daemon's startup sequence. Subscribing from `main` before any
/// subsystem task spawns guarantees the dbus service sees every
/// `InterfaceDiscovered` event.
pub async fn spawn_dbus_service(
    event_rx: broadcast::Receiver<NexusEvent>,
    profile_store: Arc<dyn ProfileStore>,
    config: DbusConfig,
) -> crate::errors::Result<DbusServiceHandle> {
    let state = Arc::new(RwLock::new(State::new(&config.version)));

    hydrate_profiles(&state, &*profile_store).await?;

    let rate_limiter = Arc::new(RateLimiter::new(config.rate_limits));
    let services = Arc::new(Services::new(
        Arc::clone(&state),
        Arc::clone(&profile_store),
        Arc::clone(&config.auth),
        Arc::clone(&config.ops),
        rate_limiter,
        config.enabled_features,
        config.ethernet_auth_backend.clone(),
        config.wifi_supplicant.clone(),
        config.wifi_roaming_mode.clone(),
    ));

    let (registry_tx, registry_rx) = mpsc::channel::<ServiceCommand>(64);

    let builder = if let Some(addr) = config.address.as_deref() {
        zbus::connection::Builder::address(addr)?
    } else if config.use_session_bus {
        zbus::connection::Builder::session()?
    } else {
        zbus::connection::Builder::system()?
    };
    let connection = builder
        .name(config.bus_name.as_str())?
        .serve_at(
            MANAGER_PATH,
            Manager::new(Arc::clone(&services), registry_tx.clone()),
        )?
        .serve_at(MANAGER_PATH, ObjectManager::new(Arc::clone(&state)))?
        .build()
        .await?;

    info!(bus = %config.bus_name, path = MANAGER_PATH, "d-bus service ready");

    // Register profile objects loaded from the store.
    let wifi_ids: Vec<Ulid> = {
        let guard = state.read().await;
        guard.wifi_profiles.values().map(|p| p.id).collect()
    };
    for id in wifi_ids {
        register_wifi_profile(&connection, &services, registry_tx.clone(), id).await?;
    }
    let eth_ids: Vec<Ulid> = {
        let guard = state.read().await;
        guard.ethernet_profiles.values().map(|p| p.id).collect()
    };
    for id in eth_ids {
        register_ethernet_profile(&connection, &services, registry_tx.clone(), id).await?;
    }

    let shutdown = CancellationToken::new();
    let shutdown_child = shutdown.clone();
    let conn_clone = connection.clone();
    let state_clone = Arc::clone(&state);
    let services_clone = Arc::clone(&services);
    let registry_tx_for_loop = registry_tx.clone();
    let join = tokio::spawn(async move {
        run_loop(
            conn_clone,
            state_clone,
            services_clone,
            registry_tx_for_loop,
            event_rx,
            registry_rx,
            shutdown_child,
        )
        .await
    });

    Ok(DbusServiceHandle {
        connection,
        state,
        services,
        shutdown,
        registry_tx,
        join,
    })
}

async fn hydrate_profiles(
    state: &Arc<RwLock<State>>,
    store: &dyn ProfileStore,
) -> crate::errors::Result<()> {
    let wifi = store.load_wifi().await.unwrap_or_default();
    let eth = store.load_ethernet().await.unwrap_or_default();
    let bt = store.load_bluetooth().await.unwrap_or_default();
    let mut guard = state.write().await;
    for p in wifi {
        guard.wifi_profiles.insert(p.id.to_string(), p);
    }
    for p in eth {
        guard.ethernet_profiles.insert(p.id.to_string(), p);
    }
    for p in bt {
        guard.bluetooth_profiles.insert(p.id.to_string(), p);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Combined event + command loop
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    connection: zbus::Connection,
    state: Arc<RwLock<State>>,
    services: Arc<Services>,
    registry_tx: mpsc::Sender<ServiceCommand>,
    mut event_rx: broadcast::Receiver<NexusEvent>,
    mut registry_rx: mpsc::Receiver<ServiceCommand>,
    shutdown: CancellationToken,
) -> crate::errors::Result<()> {
    // Coalescing flush tick — fires every COALESCE_WINDOW so any
    // bucket that was armed at least one window ago gets emitted.
    let mut flush_tick = tokio::time::interval(COALESCE_WINDOW);
    flush_tick.tick().await;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                // Final flush so clients see the last batch.
                flush_due_properties(&connection, &services, true).await;
                info!("d-bus service shutting down");
                return Ok(());
            }
            cmd = registry_rx.recv() => match cmd {
                Some(cmd) => {
                    if let Err(e) = handle_service_command(
                        &connection,
                        &services,
                        registry_tx.clone(),
                        cmd,
                    )
                    .await
                    {
                        warn!(error = ?e, "d-bus registry-command error");
                    }
                }
                None => return Ok(()),
            },
            res = event_rx.recv() => match res {
                Ok(event) => {
                    if let Err(e) = handle_event(
                        &connection,
                        &state,
                        &services,
                        registry_tx.clone(),
                        event,
                    )
                    .await
                    {
                        warn!(error = ?e, "d-bus event handler error");
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "d-bus event bus receiver lagged");
                }
            },
            _ = flush_tick.tick() => {
                flush_due_properties(&connection, &services, false).await;
            }
        }
    }
}

async fn flush_due_properties(
    connection: &zbus::Connection,
    services: &Arc<Services>,
    drain_all: bool,
) {
    let now = tokio::time::Instant::now().into_std();
    let buckets = if drain_all {
        services.batcher.drain_all()
    } else {
        services.batcher.pop_due(now, COALESCE_WINDOW)
    };
    for (path, iface, props) in buckets {
        // Use the standard `org.freedesktop.DBus.Properties.PropertiesChanged`
        // signal. For phase 8, we publish the property names in
        // the `invalidated_properties` array — clients re-read
        // them via Properties.Get. This matches DD-006 §12.1
        // semantics ("the value shown via `Properties.Get` is
        // always current; only the signal frequency is throttled").
        let Ok(obj_path) = zbus::zvariant::ObjectPath::try_from(path.clone()) else {
            continue;
        };
        let changed: std::collections::HashMap<String, zbus::zvariant::Value<'_>> =
            std::collections::HashMap::new();
        let invalidated: Vec<String> = props.into_iter().collect();
        if let Err(e) = connection
            .emit_signal(
                None::<&str>,
                obj_path.clone(),
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(iface.as_str(), changed, invalidated),
            )
            .await
        {
            tracing::debug!(error = ?e, %path, "PropertiesChanged emit failed");
        }
    }
}

async fn handle_service_command(
    connection: &zbus::Connection,
    services: &Arc<Services>,
    registry_tx: mpsc::Sender<ServiceCommand>,
    cmd: ServiceCommand,
) -> crate::errors::Result<()> {
    match cmd {
        ServiceCommand::RegisterWifiProfile(id) => {
            register_wifi_profile(connection, services, registry_tx, id).await?;
        }
        ServiceCommand::RegisterEthernetProfile(id) => {
            register_ethernet_profile(connection, services, registry_tx, id).await?;
        }
        ServiceCommand::UnregisterWifiProfile(id) => {
            unregister_wifi_profile(connection, id).await?;
        }
        ServiceCommand::UnregisterEthernetProfile(id) => {
            unregister_ethernet_profile(connection, id).await?;
        }
        ServiceCommand::RegisterScanResult { ifname, bssid } => {
            register_scan_result(connection, services, &ifname, bssid).await?;
        }
        ServiceCommand::UnregisterScanResult { ifname, bssid } => {
            unregister_scan_result(connection, &ifname, bssid).await?;
        }
    }
    Ok(())
}

async fn handle_event(
    connection: &zbus::Connection,
    state: &Arc<RwLock<State>>,
    services: &Arc<Services>,
    registry_tx: mpsc::Sender<ServiceCommand>,
    event: NexusEvent,
) -> crate::errors::Result<()> {
    match event {
        NexusEvent::InterfaceDiscovered(info) => {
            let ifname = info.ifname.clone();
            let was_present = state.read().await.interfaces.contains_key(&ifname);
            {
                let mut guard = state.write().await;
                guard
                    .interfaces
                    .insert(ifname.clone(), InterfaceState::new(info));
                if let Some(e) = guard.interfaces.get_mut(&ifname) {
                    match &mut e.kind_data {
                        InterfaceKindData::Ethernet(c) => {
                            c.auth_backend = services.ethernet_auth_backend.clone();
                        }
                        InterfaceKindData::Wifi(c) => {
                            // DD-006 §6.3 / DD-008 §4.1: stamp the
                            // configured supplicant + initial roaming
                            // mode onto the cache so property reads
                            // (and `nexusctl wifi show`) return the
                            // configured strings instead of `""`.
                            c.supplicant = services.wifi_supplicant.clone();
                            c.roaming_mode = services.wifi_roaming_mode.clone();
                        }
                        _ => {}
                    }
                }
            }
            if !was_present {
                register_interface(connection, services, &ifname).await?;
            }
        }
        NexusEvent::InterfaceRemoved { ifindex } => {
            let to_remove = {
                let guard = state.read().await;
                guard
                    .interfaces
                    .values()
                    .find(|e| e.info.ifindex == ifindex)
                    .map(|e| e.info.ifname.clone())
            };
            if let Some(ifname) = to_remove {
                unregister_interface(connection, state, &ifname).await?;
                state.write().await.interfaces.remove(&ifname);
            }
        }
        NexusEvent::CarrierChanged { ifindex, up } => {
            if let Some(ifname) = state_lookup_ifname_by_ifindex(state, ifindex).await {
                if let Some(e) = state.write().await.interfaces.get_mut(&ifname) {
                    e.info.carrier = up;
                }
            }
        }
        NexusEvent::OperstateChanged { ifindex, state: op } => {
            if let Some(ifname) = state_lookup_ifname_by_ifindex(state, ifindex).await {
                if let Some(e) = state.write().await.interfaces.get_mut(&ifname) {
                    e.info.operstate = op;
                }
            }
        }
        NexusEvent::WifiRfkillChanged { ifindex, powered } => {
            if let Some(ifname) = state_lookup_ifname_by_ifindex(state, ifindex).await {
                if let Some(e) = state.write().await.interfaces.get_mut(&ifname) {
                    if let InterfaceKindData::Wifi(c) = &mut e.kind_data {
                        c.powered = powered;
                    }
                }
            }
        }
        NexusEvent::WifiStateChanged {
            ifindex,
            state: wifi_state,
        } => {
            if let Some(ifname) = state_lookup_ifname_by_ifindex(state, ifindex).await {
                let managed_profile = {
                    let mut guard = state.write().await;
                    let mp = guard
                        .interfaces
                        .get(&ifname)
                        .and_then(|e| e.managed_profile.clone());
                    if let Some(e) = guard.interfaces.get_mut(&ifname) {
                        if let InterfaceKindData::Wifi(c) = &mut e.kind_data {
                            c.apply_state(&wifi_state);
                        }
                    }
                    mp
                };
                let (label, details) = wifi_state_label_and_details(
                    &wifi_state,
                    managed_profile.as_deref(),
                );
                emit_wifi_state_changed(connection, &ifname, &label, &details).await;
                resolve_connect_job(connection, services, &ifname, &wifi_state).await;
            }
        }
        NexusEvent::WifiSignalPoll {
            ifindex,
            rssi,
            frequency,
        } => {
            if let Some(ifname) = state_lookup_ifname_by_ifindex(state, ifindex).await {
                if let Some(e) = state.write().await.interfaces.get_mut(&ifname) {
                    if let InterfaceKindData::Wifi(c) = &mut e.kind_data {
                        c.signal_dbm = rssi;
                        c.frequency = frequency;
                    }
                }
                // Mark the hot properties dirty — the
                // coalescing tick batches them.
                let path = interface_path(&ifname);
                services.batcher.mark(&path, "fi.nexus.Wifi", "SignalDbm");
                services.batcher.mark(&path, "fi.nexus.Wifi", "Frequency");
            }
        }
        NexusEvent::WifiNetworkRequest {
            ifindex,
            network,
            field,
            text,
        } => {
            // DD-006 §9 wifi `NetworkRequest` signal. We don't
            // cache the request — clients answer once and the
            // supplicant resumes the auth flow; a stale request
            // is on its own retry path. The signal payload is
            // (network: o, field: s, text: s).
            if let Some(ifname) = state_lookup_ifname_by_ifindex(state, ifindex).await {
                let path = interface_path(&ifname);
                let Ok(obj_path) = zbus::zvariant::ObjectPath::try_from(path.clone()) else {
                    return Ok(());
                };
                let Ok(network_path) = zbus::zvariant::ObjectPath::try_from(network.clone()) else {
                    tracing::warn!(
                        ifname = %ifname,
                        network = %network,
                        "wifi NetworkRequest: bad supplicant path; dropping"
                    );
                    return Ok(());
                };
                if let Err(e) = connection
                    .emit_signal(
                        None::<&str>,
                        obj_path,
                        "fi.nexus.Wifi",
                        "NetworkRequest",
                        &(network_path, field, text),
                    )
                    .await
                {
                    tracing::debug!(
                        error = ?e,
                        %path,
                        "wifi NetworkRequest emit failed"
                    );
                }
            }
        }
        NexusEvent::WifiScanComplete {
            ifindex,
            success,
            results,
        } => {
            if let Some(ifname) = state_lookup_ifname_by_ifindex(state, ifindex).await {
                let mut to_register: Vec<nexus_core::MacAddr> = Vec::new();
                let mut to_unregister: Vec<nexus_core::MacAddr> = Vec::new();
                {
                    let mut guard = state.write().await;
                    if let Some(e) = guard.interfaces.get_mut(&ifname) {
                        if let InterfaceKindData::Wifi(c) = &mut e.kind_data {
                            // Diff old vs new BSSID set.
                            let mut new_set: std::collections::HashMap<
                                nexus_core::MacAddr,
                                nexus_core::BssInfo,
                            > = std::collections::HashMap::new();
                            for bss in results {
                                new_set.insert(bss.bssid, bss);
                            }
                            for bssid in c.scan_cache.keys() {
                                if !new_set.contains_key(bssid) {
                                    to_unregister.push(*bssid);
                                }
                            }
                            for bssid in new_set.keys() {
                                if !c.scan_cache.contains_key(bssid) {
                                    to_register.push(*bssid);
                                }
                            }
                            c.scan_cache = new_set;
                            c.scan_results = c
                                .scan_cache
                                .keys()
                                .map(|m| scan_result_path(&ifname, m))
                                .collect();
                        }
                    }
                }
                for bssid in to_register {
                    let _ = registry_tx
                        .send(ServiceCommand::RegisterScanResult {
                            ifname: ifname.clone(),
                            bssid,
                        })
                        .await;
                }
                for bssid in to_unregister {
                    let _ = registry_tx
                        .send(ServiceCommand::UnregisterScanResult {
                            ifname: ifname.clone(),
                            bssid,
                        })
                        .await;
                }
                resolve_scan_job(connection, services, &ifname, success).await;
            }
        }
        NexusEvent::EthLifecycleStateChanged {
            ifindex,
            state: state_label,
            eap_method,
            auth_failure_reason,
        } => {
            if let Some(ifname) = state_lookup_ifname_by_ifindex(state, ifindex).await {
                let managed_profile = {
                    let mut guard = state.write().await;
                    let mp = guard.interfaces.get(&ifname).and_then(|e| e.managed_profile.clone());
                    if let Some(e) = guard.interfaces.get_mut(&ifname) {
                        if let InterfaceKindData::Ethernet(c) = &mut e.kind_data {
                            c.state = state_label.clone();
                            c.eap_method = eap_method.clone().unwrap_or_default();
                            c.auth_failure_reason =
                                auth_failure_reason.clone().unwrap_or_default();
                        }
                    }
                    mp
                };
                // DD-006 §6.2 / §9: ethernet has no
                // technology-specific signals; lifecycle transitions
                // surface through `fi.nexus.Interface.StateChanged`
                // with `reason` / `eap_method` / `profile` in details.
                emit_eth_state_changed(
                    connection,
                    &ifname,
                    &state_label,
                    eap_method.as_deref(),
                    auth_failure_reason.as_deref(),
                    managed_profile.as_deref(),
                )
                .await;
            }
        }
        NexusEvent::BtAdapterChanged {
            adapter,
            powered,
            discovering,
        } => {
            if let Some(ifname) = state_lookup_bluetooth_ifname(state, &adapter).await {
                if let Some(e) = state.write().await.interfaces.get_mut(&ifname) {
                    if let InterfaceKindData::Bluetooth(c) = &mut e.kind_data {
                        c.powered = powered;
                        c.discovering = discovering;
                        c.state = if discovering {
                            "discovering"
                        } else if powered {
                            "powered"
                        } else {
                            "present"
                        }
                        .to_owned();
                    }
                }
            }
        }
        event @ (NexusEvent::GnssTpvReceived { .. }
        | NexusEvent::GnssSatellites { .. }
        | NexusEvent::GnssFixChanged { .. }
        | NexusEvent::GnssGpsdConnected
        | NexusEvent::GnssGpsdDisconnected) => {
            apply_gnss_event(state, event).await;
        }
        NexusEvent::OperatorNotification { kind, data } => {
            debug!(kind, data_len = data.len(), "operator notification");
            emit_manager_notification_event(connection, &kind, &data).await;
        }
        NexusEvent::InternetConnectivityChanged { state: next } => {
            let prev = {
                let mut guard = state.write().await;
                let prev = guard.internet_connectivity;
                guard.internet_connectivity = next;
                prev
            };
            // Defensive: the probe already de-dupes, but a probe
            // restart or a stray event shouldn't double-emit.
            if prev != next {
                debug!(
                    from = prev.as_str(),
                    to = next.as_str(),
                    "internet connectivity transition"
                );
                emit_manager_internet_connectivity_changed(connection, next.as_str()).await;
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Registration helpers — interfaces / profiles
// ---------------------------------------------------------------------------

async fn register_interface(
    connection: &zbus::Connection,
    services: &Arc<Services>,
    ifname: &str,
) -> crate::errors::Result<()> {
    let path = interface_path(ifname);
    let obj_path = ObjectPath::try_from(path.clone())
        .map_err(|e| crate::errors::DbusError::InvalidArgument(format!("{e}")))?;
    let owned = OwnedObjectPath::from(obj_path);

    let srv = connection.object_server();
    srv.at(
        owned.clone(),
        InterfaceIface::new(Arc::clone(services), ifname),
    )
    .await?;

    let kind_tag = {
        let guard = services.state.read().await;
        guard.interfaces.get(ifname).map(|e| kind_tag(&e.kind_data))
    };
    match kind_tag {
        Some("ethernet") => {
            srv.at(
                owned.clone(),
                EthernetIface::new(Arc::clone(services), ifname),
            )
            .await?;
        }
        Some("wifi") => {
            srv.at(owned.clone(), WifiIface::new(Arc::clone(services), ifname))
                .await?;
        }
        Some("bluetooth") => {
            srv.at(
                owned.clone(),
                BluetoothIface::new(Arc::clone(services), ifname),
            )
            .await?;
        }
        Some("gnss") => {
            srv.at(owned.clone(), GnssIface::new(Arc::clone(services), ifname))
                .await?;
        }
        _ => {}
    }

    let ifaces: IfaceMap = {
        let guard = services.state.read().await;
        match guard.interfaces.get(ifname) {
            Some(e) => {
                let mut m: IfaceMap = std::collections::HashMap::new();
                m.insert(
                    iface_names::COMMON.to_owned(),
                    object_manager::common_props(ifname, e),
                );
                match &e.kind_data {
                    InterfaceKindData::Ethernet(c) => {
                        m.insert(
                            iface_names::ETHERNET.to_owned(),
                            object_manager::ethernet_props(c),
                        );
                    }
                    InterfaceKindData::Wifi(c) => {
                        m.insert(iface_names::WIFI.to_owned(), object_manager::wifi_props(c));
                    }
                    InterfaceKindData::Bluetooth(c) => {
                        m.insert(
                            iface_names::BLUETOOTH.to_owned(),
                            object_manager::bluetooth_props(ifname, c),
                        );
                    }
                    InterfaceKindData::Gnss(c) => {
                        m.insert(iface_names::GNSS.to_owned(), object_manager::gnss_props(c));
                    }
                }
                m
            }
            None => IfaceMap::new(),
        }
    };
    if let Ok(mgr_ref) = srv.interface::<_, ObjectManager>(MANAGER_PATH).await {
        let _ = ObjectManager::interfaces_added(mgr_ref.signal_emitter(), owned, ifaces).await;
    }
    Ok(())
}

async fn unregister_interface(
    connection: &zbus::Connection,
    state: &Arc<RwLock<State>>,
    ifname: &str,
) -> crate::errors::Result<()> {
    let path = interface_path(ifname);
    let obj_path = ObjectPath::try_from(path.clone())
        .map_err(|e| crate::errors::DbusError::InvalidArgument(format!("{e}")))?;
    let owned = OwnedObjectPath::from(obj_path);
    let srv = connection.object_server();

    let mut names = vec![iface_names::COMMON.to_owned()];
    let kind = {
        let guard = state.read().await;
        guard.interfaces.get(ifname).map(|e| kind_tag(&e.kind_data))
    };
    if let Some(tag) = kind {
        names.push(
            match tag {
                "ethernet" => iface_names::ETHERNET,
                "wifi" => iface_names::WIFI,
                "bluetooth" => iface_names::BLUETOOTH,
                "gnss" => iface_names::GNSS,
                _ => iface_names::COMMON,
            }
            .to_owned(),
        );
    }

    let _ = srv.remove::<InterfaceIface, _>(owned.clone()).await;
    let _ = srv.remove::<EthernetIface, _>(owned.clone()).await;
    let _ = srv.remove::<WifiIface, _>(owned.clone()).await;
    let _ = srv.remove::<BluetoothIface, _>(owned.clone()).await;
    let _ = srv.remove::<GnssIface, _>(owned.clone()).await;

    if let Ok(mgr_ref) = srv.interface::<_, ObjectManager>(MANAGER_PATH).await {
        let _ = ObjectManager::interfaces_removed(mgr_ref.signal_emitter(), owned, names).await;
    }
    Ok(())
}

async fn register_wifi_profile(
    connection: &zbus::Connection,
    services: &Arc<Services>,
    registry_tx: mpsc::Sender<ServiceCommand>,
    id: Ulid,
) -> crate::errors::Result<()> {
    let path = wifi_profile_path(&id);
    let obj_path = ObjectPath::try_from(path.clone())
        .map_err(|e| crate::errors::DbusError::InvalidArgument(format!("{e}")))?;
    let owned = OwnedObjectPath::from(obj_path);
    let srv = connection.object_server();
    srv.at(
        owned.clone(),
        ProfileIface::new(
            Arc::clone(services),
            registry_tx.clone(),
            id,
            ProfileKind::Wifi,
        ),
    )
    .await?;
    srv.at(
        owned.clone(),
        WifiProfileIface::new(Arc::clone(services), id),
    )
    .await?;

    let ifaces: IfaceMap = {
        let guard = services.state.read().await;
        let mut m: IfaceMap = std::collections::HashMap::new();
        if let Some(p) = guard.wifi_profiles.get(&id.to_string()) {
            m.insert(
                prof_iface_names::COMMON.to_owned(),
                object_manager::profile_common_props_wifi(p),
            );
            m.insert(
                prof_iface_names::WIFI.to_owned(),
                object_manager::profile_wifi_props(p),
            );
        }
        m
    };
    if let Ok(mgr_ref) = srv.interface::<_, ObjectManager>(MANAGER_PATH).await {
        let _ = ObjectManager::interfaces_added(mgr_ref.signal_emitter(), owned, ifaces).await;
    }
    Ok(())
}

async fn unregister_wifi_profile(
    connection: &zbus::Connection,
    id: Ulid,
) -> crate::errors::Result<()> {
    let path = wifi_profile_path(&id);
    let obj_path = ObjectPath::try_from(path.clone())
        .map_err(|e| crate::errors::DbusError::InvalidArgument(format!("{e}")))?;
    let owned = OwnedObjectPath::from(obj_path);
    let srv = connection.object_server();
    let _ = srv.remove::<ProfileIface, _>(owned.clone()).await;
    let _ = srv.remove::<WifiProfileIface, _>(owned.clone()).await;
    let names = vec![
        prof_iface_names::COMMON.to_owned(),
        prof_iface_names::WIFI.to_owned(),
    ];
    if let Ok(mgr_ref) = srv.interface::<_, ObjectManager>(MANAGER_PATH).await {
        let _ = ObjectManager::interfaces_removed(mgr_ref.signal_emitter(), owned, names).await;
    }
    Ok(())
}

async fn register_ethernet_profile(
    connection: &zbus::Connection,
    services: &Arc<Services>,
    registry_tx: mpsc::Sender<ServiceCommand>,
    id: Ulid,
) -> crate::errors::Result<()> {
    let path = ethernet_profile_path(&id);
    let obj_path = ObjectPath::try_from(path.clone())
        .map_err(|e| crate::errors::DbusError::InvalidArgument(format!("{e}")))?;
    let owned = OwnedObjectPath::from(obj_path);
    let srv = connection.object_server();
    srv.at(
        owned.clone(),
        ProfileIface::new(
            Arc::clone(services),
            registry_tx.clone(),
            id,
            ProfileKind::Ethernet,
        ),
    )
    .await?;
    srv.at(
        owned.clone(),
        EthernetProfileIface::new(Arc::clone(services), id),
    )
    .await?;

    let ifaces: IfaceMap = {
        let guard = services.state.read().await;
        let mut m: IfaceMap = std::collections::HashMap::new();
        if let Some(p) = guard.ethernet_profiles.get(&id.to_string()) {
            m.insert(
                prof_iface_names::COMMON.to_owned(),
                object_manager::profile_common_props_ethernet(p),
            );
            m.insert(
                prof_iface_names::ETHERNET.to_owned(),
                object_manager::profile_ethernet_props(p),
            );
        }
        m
    };
    if let Ok(mgr_ref) = srv.interface::<_, ObjectManager>(MANAGER_PATH).await {
        let _ = ObjectManager::interfaces_added(mgr_ref.signal_emitter(), owned, ifaces).await;
    }
    Ok(())
}

async fn register_scan_result(
    connection: &zbus::Connection,
    services: &Arc<Services>,
    ifname: &str,
    bssid: nexus_core::MacAddr,
) -> crate::errors::Result<()> {
    let path = scan_result_path(ifname, &bssid);
    let obj_path = ObjectPath::try_from(path.clone())
        .map_err(|e| crate::errors::DbusError::InvalidArgument(format!("{e}")))?;
    let owned = OwnedObjectPath::from(obj_path);
    let srv = connection.object_server();
    srv.at(
        owned.clone(),
        ScanResultIface::new(Arc::clone(services), ifname, bssid),
    )
    .await?;
    let mut ifaces: IfaceMap = std::collections::HashMap::new();
    let mut props: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        std::collections::HashMap::new();
    if let Ok(v) =
        zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::new(bssid.0.to_vec()))
    {
        props.insert("Bssid".to_owned(), v);
    }
    ifaces.insert("fi.nexus.ScanResult".to_owned(), props);
    if let Ok(mgr_ref) = srv.interface::<_, ObjectManager>(MANAGER_PATH).await {
        let _ = ObjectManager::interfaces_added(mgr_ref.signal_emitter(), owned, ifaces).await;
    }
    Ok(())
}

async fn unregister_scan_result(
    connection: &zbus::Connection,
    ifname: &str,
    bssid: nexus_core::MacAddr,
) -> crate::errors::Result<()> {
    let path = scan_result_path(ifname, &bssid);
    let obj_path = ObjectPath::try_from(path.clone())
        .map_err(|e| crate::errors::DbusError::InvalidArgument(format!("{e}")))?;
    let owned = OwnedObjectPath::from(obj_path);
    let srv = connection.object_server();
    let _ = srv.remove::<ScanResultIface, _>(owned.clone()).await;
    if let Ok(mgr_ref) = srv.interface::<_, ObjectManager>(MANAGER_PATH).await {
        let _ = ObjectManager::interfaces_removed(
            mgr_ref.signal_emitter(),
            owned,
            vec!["fi.nexus.ScanResult".to_owned()],
        )
        .await;
    }
    Ok(())
}

async fn unregister_ethernet_profile(
    connection: &zbus::Connection,
    id: Ulid,
) -> crate::errors::Result<()> {
    let path = ethernet_profile_path(&id);
    let obj_path = ObjectPath::try_from(path.clone())
        .map_err(|e| crate::errors::DbusError::InvalidArgument(format!("{e}")))?;
    let owned = OwnedObjectPath::from(obj_path);
    let srv = connection.object_server();
    let _ = srv.remove::<ProfileIface, _>(owned.clone()).await;
    let _ = srv.remove::<EthernetProfileIface, _>(owned.clone()).await;
    let names = vec![
        prof_iface_names::COMMON.to_owned(),
        prof_iface_names::ETHERNET.to_owned(),
    ];
    if let Ok(mgr_ref) = srv.interface::<_, ObjectManager>(MANAGER_PATH).await {
        let _ = ObjectManager::interfaces_removed(mgr_ref.signal_emitter(), owned, names).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// State lookups + side effects
// ---------------------------------------------------------------------------

async fn state_lookup_ifname_by_ifindex(
    state: &Arc<RwLock<State>>,
    ifindex: u32,
) -> Option<String> {
    state
        .read()
        .await
        .interfaces
        .values()
        .find(|e| e.info.ifindex == ifindex)
        .map(|e| e.info.ifname.clone())
}

async fn state_lookup_bluetooth_ifname(
    state: &Arc<RwLock<State>>,
    bluez_path: &str,
) -> Option<String> {
    state
        .read()
        .await
        .interfaces
        .values()
        .find_map(|e| match &e.info.kind {
            InterfaceKind::Bluetooth {
                bluez_path: p,
                hci_name,
                ..
            } if p == bluez_path => Some(hci_name.clone()),
            _ => None,
        })
}

fn kind_tag(k: &InterfaceKindData) -> &'static str {
    match k {
        InterfaceKindData::Ethernet(_) => "ethernet",
        InterfaceKindData::Wifi(_) => "wifi",
        InterfaceKindData::Bluetooth(_) => "bluetooth",
        InterfaceKindData::Gnss(_) => "gnss",
    }
}

/// Emit `fi.nexus.Manager.NotificationEvent(kind: s, data: a{sv})`
/// (DD-006 §5.3). Translates a [`NotificationData`] payload to the
/// D-Bus dict shape. Best-effort — a failure to emit is logged at
/// debug.
async fn emit_manager_notification_event(
    connection: &zbus::Connection,
    kind: &str,
    data: &nexus_core::NotificationData,
) {
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    let mut payload: HashMap<String, OwnedValue> = HashMap::new();
    for (k, v) in data.iter() {
        let value = match v {
            nexus_core::NotificationValue::String(s) => OwnedValue::try_from(Value::new(s.clone())),
            nexus_core::NotificationValue::U32(n) => OwnedValue::try_from(Value::new(*n)),
            nexus_core::NotificationValue::U64(n) => OwnedValue::try_from(Value::new(*n)),
            nexus_core::NotificationValue::Bool(b) => OwnedValue::try_from(Value::new(*b)),
            nexus_core::NotificationValue::ObjectPath(p) => match ObjectPath::try_from(p.clone()) {
                Ok(op) => OwnedValue::try_from(Value::ObjectPath(op)),
                Err(_) => continue,
            },
        };
        if let Ok(v) = value {
            payload.insert(k.clone(), v);
        }
    }
    let Ok(manager_path) = ObjectPath::try_from(MANAGER_PATH) else {
        return;
    };
    if let Err(e) = connection
        .emit_signal(
            None::<&str>,
            manager_path,
            "fi.nexus.Manager",
            "NotificationEvent",
            &(kind, payload),
        )
        .await
    {
        tracing::debug!(error = ?e, kind, "Manager.NotificationEvent emit failed");
    }
}

/// Emit `fi.nexus.Manager.InternetConnectivityChanged(state: s)`.
/// Best-effort — a failure to emit is logged at debug.
async fn emit_manager_internet_connectivity_changed(connection: &zbus::Connection, state: &str) {
    let Ok(manager_path) = ObjectPath::try_from(MANAGER_PATH) else {
        return;
    };
    if let Err(e) = connection
        .emit_signal(
            None::<&str>,
            manager_path,
            "fi.nexus.Manager",
            "InternetConnectivityChanged",
            &(state,),
        )
        .await
    {
        tracing::debug!(error = ?e, state, "Manager.InternetConnectivityChanged emit failed");
    }
}

/// Emit `fi.nexus.Interface.StateChanged(new_state: s, details: a{sv})`
/// for an Ethernet interface (DD-006 §6.2 / §9). Best-effort — a
/// failure to emit is logged at debug.
async fn emit_eth_state_changed(
    connection: &zbus::Connection,
    ifname: &str,
    state_label: &str,
    eap_method: Option<&str>,
    auth_failure_reason: Option<&str>,
    managed_profile: Option<&str>,
) {
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    let path = interface_path(ifname);
    let Ok(obj_path) = ObjectPath::try_from(path.clone()) else {
        return;
    };
    let mut details: HashMap<String, OwnedValue> = HashMap::new();
    if let Some(reason) = auth_failure_reason {
        if !reason.is_empty() {
            if let Ok(v) = OwnedValue::try_from(Value::new(reason.to_owned())) {
                details.insert("reason".into(), v);
            }
        }
    }
    // DD-006 §9 details table: `eap_method` is set when the
    // interface enters `Authenticating`. For other auth-relevant
    // states the cached property carries it; we keep the signal
    // payload focused on the authoritative table.
    if state_label == "authenticating" {
        if let Some(eap) = eap_method {
            if !eap.is_empty() {
                if let Ok(v) = OwnedValue::try_from(Value::new(eap.to_owned())) {
                    details.insert("eap_method".into(), v);
                }
            }
        }
    }
    if let Some(profile) = managed_profile {
        if profile != "/" && !profile.is_empty() {
            if let Ok(p) = ObjectPath::try_from(profile.to_owned()) {
                if let Ok(v) = OwnedValue::try_from(Value::ObjectPath(p)) {
                    details.insert("profile".into(), v);
                }
            }
        }
    }
    if let Err(e) = connection
        .emit_signal(
            None::<&str>,
            obj_path,
            "fi.nexus.Interface",
            "StateChanged",
            &(state_label, details),
        )
        .await
    {
        tracing::debug!(
            error = ?e,
            ifname,
            state = state_label,
            "fi.nexus.Interface.StateChanged emit failed",
        );
    }
}


/// Build the `(state_label, details)` payload for a Wi-Fi state
/// transition (DD-006 §9). The same struct is emitted on
/// `fi.nexus.Wifi.StateChanged` and `fi.nexus.Interface.StateChanged`
/// so client UIs can subscribe to either channel.
fn wifi_state_label_and_details(
    s: &nexus_core::WifiState,
    managed_profile: Option<&str>,
) -> (String, std::collections::HashMap<String, zbus::zvariant::OwnedValue>) {
    use nexus_core::WifiState;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    let label = match s {
        WifiState::Idle => "idle",
        WifiState::Scanning => "scanning",
        WifiState::Connecting { .. } => "connecting",
        WifiState::Authenticating { .. } => "authenticating",
        WifiState::Handshaking { .. } => "handshaking",
        WifiState::Connected { .. } => "connected",
        WifiState::Roaming { .. } => "roaming",
        WifiState::Disconnected { .. } => "disconnected",
        WifiState::Gone => "gone",
    }
    .to_owned();

    let mut details: HashMap<String, OwnedValue> = HashMap::new();

    let insert_bssid = |details: &mut HashMap<String, OwnedValue>, bssid: &nexus_core::MacAddr| {
        if let Ok(v) = OwnedValue::try_from(Value::new(bssid.0.to_vec())) {
            details.insert("bssid".into(), v);
        }
    };
    let insert_ssid = |details: &mut HashMap<String, OwnedValue>, ssid: &nexus_core::Ssid| {
        if let Ok(v) = OwnedValue::try_from(Value::new(ssid.as_bytes().to_vec())) {
            details.insert("ssid_bytes".into(), v);
        }
    };

    match s {
        WifiState::Connecting { bssid, ssid }
        | WifiState::Authenticating { bssid, ssid }
        | WifiState::Handshaking { bssid, ssid } => {
            insert_bssid(&mut details, bssid);
            insert_ssid(&mut details, ssid);
        }
        WifiState::Connected {
            bssid,
            ssid,
            frequency,
            signal_dbm,
            security,
        } => {
            insert_bssid(&mut details, bssid);
            insert_ssid(&mut details, ssid);
            if let Ok(v) = OwnedValue::try_from(Value::new(*frequency)) {
                details.insert("frequency".into(), v);
            }
            if let Ok(v) = OwnedValue::try_from(Value::new(*signal_dbm)) {
                details.insert("signal_dbm".into(), v);
            }
            if let Ok(v) = OwnedValue::try_from(Value::new(crate::state::security_label(*security)))
            {
                details.insert("security".into(), v);
            }
        }
        WifiState::Roaming { from: _, to, ssid } => {
            insert_bssid(&mut details, to);
            insert_ssid(&mut details, ssid);
        }
        WifiState::Disconnected { reason } => {
            if let Ok(v) = OwnedValue::try_from(Value::new(disconnect_reason_wire(reason).to_owned()))
            {
                details.insert("reason".into(), v);
            }
        }
        _ => {}
    }

    if let Some(profile) = managed_profile {
        if profile != "/" && !profile.is_empty() {
            if let Ok(p) = ObjectPath::try_from(profile.to_owned()) {
                if let Ok(v) = OwnedValue::try_from(Value::ObjectPath(p)) {
                    details.insert("profile".into(), v);
                }
            }
        }
    }

    (label, details)
}

/// Reasons on which `Wifi.ConnectComplete` resolves a pending
/// Connect job. Anything outside this set is treated as transient —
/// the wifi backend's cooldown + sticky-retry path will produce a
/// converged outcome eventually (either a successful `Connected` or
/// a promoted `CredentialsInvalid` after the BSSID failure threshold
/// trips) and *that* fires `ConnectComplete`. Keeps the contract
/// promised by the integration knowledge graph: one Connect call →
/// exactly one ConnectComplete.
fn is_terminal_connect_reason(wire: &str) -> bool {
    matches!(
        wire,
        "credentials_invalid" | "rf_killed" | "supplicant_unavailable"
    )
}

/// Map a [`nexus_core::DisconnectReason`] to the wire string
/// documented in DD-006 §9 (also used by `Wifi.ConnectComplete`).
fn disconnect_reason_wire(r: &nexus_core::DisconnectReason) -> &'static str {
    use nexus_core::DisconnectReason::*;
    match r {
        CredentialsInvalid | EapFailure | AuthExpired => "credentials_invalid",
        HandshakeTimeout => "handshake_timeout",
        RfKilled => "rf_killed",
        SupplicantUnavailable => "supplicant_unavailable",
        LocalRequest => "cancelled",
        ApInitiated | Inactivity | ProtocolError | PostSleepRecovery | DriverWedge
        | Unspecified | Other(_) => "other",
    }
}

/// Emit `fi.nexus.Wifi.StateChanged(new_state: s, details: a{sv})`
/// AND `fi.nexus.Interface.StateChanged(new_state: s, details: a{sv})`
/// on the interface's object path. Both signals carry identical
/// payloads — clients pick one and ignore the other (DD-006 §9).
async fn emit_wifi_state_changed(
    connection: &zbus::Connection,
    ifname: &str,
    state_label: &str,
    details: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
) {
    let path = interface_path(ifname);
    let Ok(obj_path) = ObjectPath::try_from(path.clone()) else {
        return;
    };
    if let Err(e) = connection
        .emit_signal(
            None::<&str>,
            &obj_path,
            "fi.nexus.Wifi",
            "StateChanged",
            &(state_label, details),
        )
        .await
    {
        tracing::debug!(
            error = ?e,
            ifname,
            state = state_label,
            "fi.nexus.Wifi.StateChanged emit failed",
        );
    }
    if let Err(e) = connection
        .emit_signal(
            None::<&str>,
            &obj_path,
            "fi.nexus.Interface",
            "StateChanged",
            &(state_label, details),
        )
        .await
    {
        tracing::debug!(
            error = ?e,
            ifname,
            state = state_label,
            "fi.nexus.Interface.StateChanged (wifi) emit failed",
        );
    }
}

/// If a `Wifi.Connect` job is pending for `ifname`, resolve it
/// against this state transition and emit `Wifi.ConnectComplete`.
///
/// Resolution rules (DD-006 §6.3 — `ConnectComplete` is the
/// **terminal** edge for an operator-initiated `Connect`):
///
/// - `Connected` → success, `reason=""`.
/// - `Disconnected { reason }` → emit only when the wire reason is
///   one of the terminal set: `credentials_invalid`, `rf_killed`,
///   `supplicant_unavailable`. These are the reasons sticky-retry
///   either won't recover from on its own (`credentials_invalid`)
///   or that require operator intervention before any further
///   attempt could succeed (`rf_killed`, `supplicant_unavailable`).
/// - Every other `Disconnected { reason }` (handshake_timeout,
///   ap_initiated, post_sleep_recovery, …) is **swallowed** — the
///   wifi backend's cooldown + sticky-retry path will keep trying,
///   and the eventual converged transition (a successful `Connected`
///   or a promoted `CredentialsInvalid` after the BSSID failure
///   threshold trips) is what fires `ConnectComplete`. This means
///   exactly one `ConnectComplete` per Connect job, matching the
///   integration knowledge graph's stated contract.
/// - All other `WifiState::*` variants (Connecting, Authenticating,
///   Handshaking, Roaming, Idle, Scanning, Gone) leave the job
///   pending — none of them are terminal for a Connect attempt.
async fn resolve_connect_job(
    connection: &zbus::Connection,
    services: &Arc<Services>,
    ifname: &str,
    s: &nexus_core::WifiState,
) {
    use nexus_core::WifiState;
    let (success, reason): (bool, &str) = match s {
        WifiState::Connected { .. } => (true, ""),
        WifiState::Disconnected { reason } => {
            let wire = disconnect_reason_wire(reason);
            if !is_terminal_connect_reason(wire) {
                tracing::debug!(
                    ifname,
                    reason = wire,
                    "wifi connect: transient disconnect — holding job for sticky-retry verdict"
                );
                return;
            }
            (false, wire)
        }
        _ => return,
    };
    let Some(job_id) = services.wifi_jobs.take_connect(ifname) else {
        return;
    };
    let path = interface_path(ifname);
    let Ok(obj_path) = ObjectPath::try_from(path.clone()) else {
        return;
    };
    if let Err(e) = connection
        .emit_signal(
            None::<&str>,
            &obj_path,
            "fi.nexus.Wifi",
            "ConnectComplete",
            &(job_id.as_str(), success, reason),
        )
        .await
    {
        tracing::debug!(
            error = ?e,
            ifname,
            job_id = job_id,
            "fi.nexus.Wifi.ConnectComplete emit failed",
        );
    }
}

/// If a `Wifi.Scan` job is pending for `ifname`, resolve it against
/// this scan-complete event and emit `Wifi.ScanComplete`. The
/// `results_count` payload is read from the freshly-applied
/// `scan_cache` so it reflects the on-bus number of registered
/// `fi.nexus.ScanResult` objects rather than the raw event payload
/// — the two match in the steady state but the cache is the
/// authoritative source for what clients see.
async fn resolve_scan_job(
    connection: &zbus::Connection,
    services: &Arc<Services>,
    ifname: &str,
    success: bool,
) {
    let Some(job_id) = services.wifi_jobs.take_scan(ifname) else {
        return;
    };
    let path = interface_path(ifname);
    let Ok(obj_path) = ObjectPath::try_from(path.clone()) else {
        return;
    };
    let results_count: u32 = if success {
        let guard = services.state.read().await;
        match guard.interfaces.get(ifname).map(|e| &e.kind_data) {
            Some(InterfaceKindData::Wifi(c)) => c.scan_cache.len() as u32,
            _ => 0,
        }
    } else {
        0
    };
    let reason: &str = if success { "" } else { "aborted" };
    if let Err(e) = connection
        .emit_signal(
            None::<&str>,
            &obj_path,
            "fi.nexus.Wifi",
            "ScanComplete",
            &(job_id.as_str(), success, results_count, reason),
        )
        .await
    {
        tracing::debug!(
            error = ?e,
            ifname,
            job_id = job_id,
            "fi.nexus.Wifi.ScanComplete emit failed",
        );
    }
}

async fn apply_gnss_event(state: &Arc<RwLock<State>>, event: NexusEvent) {
    match event {
        NexusEvent::GnssTpvReceived { device, fix }
        | NexusEvent::GnssFixChanged { device, fix } => {
            if let Some(e) = state.write().await.interfaces.values_mut().find(|e| {
                matches!(&e.info.kind, InterfaceKind::Gnss { device_path, .. } if device_path == &device)
            }) {
                if let InterfaceKindData::Gnss(c) = &mut e.kind_data {
                    c.state = "tracking".into();
                    if let Some(eph) = fix.horizontal_error_m {
                        c.horizontal_error_m = eph;
                    }
                    c.satellites_used = fix.satellites_used;
                    c.last_fix = Some(fix);
                }
            }
        }
        NexusEvent::GnssSatellites { device, satellites } => {
            if let Some(e) = state.write().await.interfaces.values_mut().find(|e| {
                matches!(&e.info.kind, InterfaceKind::Gnss { device_path, .. } if device_path == &device)
            }) {
                if let InterfaceKindData::Gnss(c) = &mut e.kind_data {
                    c.satellites_in_view = satellites.len() as u32;
                    c.last_satellites = satellites;
                }
            }
        }
        NexusEvent::GnssGpsdConnected => {
            for e in state.write().await.interfaces.values_mut() {
                if let InterfaceKindData::Gnss(c) = &mut e.kind_data {
                    c.gpsd_connected = true;
                }
            }
        }
        NexusEvent::GnssGpsdDisconnected => {
            for e in state.write().await.interfaces.values_mut() {
                if let InterfaceKindData::Gnss(c) = &mut e.kind_data {
                    c.gpsd_connected = false;
                }
            }
        }
        _ => {}
    }
}
