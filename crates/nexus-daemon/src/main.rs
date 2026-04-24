//! `nexusd` — the Nexus daemon binary. Composes every subsystem
//! per `docs/nexus-architecture.md` §5.
//!
//! Responsibilities:
//!   1. Parse `--config PATH` (defaults to `/etc/nexus/nexus.toml`).
//!   2. Load + validate the TOML.
//!   3. Build a [`NexusEvent`] broadcast channel sized from config.
//!   4. Construct a [`ProfileFileStore`] (master key per config).
//!   5. For each enabled subsystem, spawn a supervised task that
//!      invokes the subsystem's `spawn_*` helper and bridges the
//!      shared shutdown token into the subsystem's private one.
//!   6. Install `SIGTERM` / `SIGINT` handlers that cancel the shared
//!      token.
//!   7. Wait for all supervised tasks to finish, then exit.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use nexus_bluetooth::{BluetoothConfig, MockBluezClient, ZbusBluezClient, spawn_bluetooth_backend};
use nexus_core::NexusEvent;
use nexus_daemon::{
    Config, LogLevelSetter, ReloadCoordinator, ReloadOps, SubsystemName, WifiBackendOps, preflight,
    spawn_bus, spawn_supervised,
};
use nexus_dbus::{
    BackendOps, DbusConfig, EnabledFeatures, NoopOps, PolicyKitChecker, RateLimits, always_allow,
    spawn_dbus_service,
};
use nexus_ethernet::{AuthBackendKind, EthernetConfig, RetryPolicy, spawn_ethernet_backend};
use nexus_gnss::{GnssConfig, GnssDefaults, JsonGpsdClient, MockGpsdClient, spawn_gnss_backend};
use nexus_interface_monitor::spawn_interface_monitor;
use nexus_profile_store::{
    FileKeySource, InMemoryKeySource, MasterKeySource, ProfileFileStore, ProfileStore,
};
use nexus_wifi::{
    WifiConfig,
    supplicant::{MockSupplicant, WifiSupplicantBackend},
};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1).collect()) {
        Ok(ParsedArgs::Run(args)) => match run(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                // Belt-and-braces: `error!` only surfaces if a
                // tracing subscriber is live — but config load + the
                // preflight step run *before* init_tracing, so an
                // error from either would silently vanish into a
                // non-existent subscriber and the operator would see
                // exit 1 with no output. Write to stderr directly
                // first, then emit the tracing event so structured
                // logs capture it when a subscriber does exist.
                eprintln!("nexusd: {e:#}");
                error!(error = %format!("{e:#}"), "nexusd startup failed");
                ExitCode::FAILURE
            }
        },
        Ok(ParsedArgs::PrintHelp) => {
            print!("{}", help_text());
            ExitCode::SUCCESS
        }
        Ok(ParsedArgs::PrintVersion) => {
            println!("nexusd {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("nexusd: {e}");
            eprintln!("try `nexusd --help` for usage");
            ExitCode::from(2)
        }
    }
}

/// Result of parsing argv. `Run` carries the effective config path;
/// `PrintHelp` / `PrintVersion` mean main should emit the canned
/// text and exit 0 without booting the daemon.
#[derive(Debug, PartialEq, Eq)]
enum ParsedArgs {
    Run(CliArgs),
    PrintHelp,
    PrintVersion,
}

#[derive(Debug, PartialEq, Eq)]
struct CliArgs {
    config_path: PathBuf,
}

fn parse_args(args: Vec<String>) -> Result<ParsedArgs> {
    let mut config_path: Option<PathBuf> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow!("--config requires a PATH argument"))?;
                config_path = Some(PathBuf::from(value));
            }
            "-h" | "--help" => return Ok(ParsedArgs::PrintHelp),
            "-V" | "--version" => return Ok(ParsedArgs::PrintVersion),
            other => {
                return Err(anyhow!("unexpected argument: {other}"));
            }
        }
    }
    Ok(ParsedArgs::Run(CliArgs {
        config_path: config_path.unwrap_or_else(|| PathBuf::from("/etc/nexus/nexus.toml")),
    }))
}

/// Human-readable `--help` text. Kept as a function (rather than a
/// `const`) so `CARGO_PKG_VERSION` is baked in at compile time while
/// the body can use multi-line formatting freely.
fn help_text() -> String {
    format!(
        "\
nexusd {version} — the Nexus platform connectivity daemon

Manages Ethernet, Wi-Fi, Bluetooth, and GNSS on embedded Linux
devices behind a single D-Bus API. See docs/nexus-architecture.md
for the full subsystem layout.

Usage:
    nexusd [OPTIONS]

Options:
        --config PATH    Path to the TOML config
                         (default: /etc/nexus/nexus.toml)
    -h, --help           Print this help and exit
    -V, --version        Print the version string and exit

Environment:
    RUST_LOG             Overrides the log_level directive from the
                         config for this run — takes tracing
                         EnvFilter syntax, e.g.
                           RUST_LOG=debug,nexus_wifi=trace
    NOTIFY_SOCKET        When set (systemd unit), nexusd sends
                         READY=1 once every subsystem is up and
                         STOPPING=1 on graceful shutdown

Signals:
    SIGTERM / SIGINT     Start a graceful shutdown — supervisors
                         stop, D-Bus name is released, exit 0

Self-test:
    On startup, nexusd probes the environment for the things each
    enabled subsystem needs (profile-store write access, BlueZ /
    wpa_supplicant / gpsd on PATH, …). Warnings print the required
    fix and the daemon continues; fatal findings print the fix and
    abort with exit 1 before any subsystem is spawned.

Exit codes:
    0    clean exit (including SIGTERM path)
    1    runtime failure during startup or supervisor join
         (includes a fatal preflight finding)
    2    usage error (bad argv)

Typical invocations:
    nexusd                                  # use /etc/nexus/nexus.toml
    nexusd --config /etc/nexus/dev.toml
    RUST_LOG=debug nexusd --config ./local.toml
",
        version = env!("CARGO_PKG_VERSION"),
    )
}

async fn run(args: CliArgs) -> Result<()> {
    let config = Config::load_from_path(&args.config_path)
        .with_context(|| format!("loading config {}", args.config_path.display()))?;
    let log_setter = init_tracing(&config.log_level);
    info!(
        path = %args.config_path.display(),
        bus_capacity = config.bus_capacity,
        "nexusd starting"
    );

    // Self-test the environment before wiring any subsystems. Fatal
    // findings abort with exit 1; warnings print action items but
    // let the daemon proceed — backend reconnect loops handle the
    // transient-external-daemon case on their own.
    let findings = preflight::run(&config);
    if !findings.is_empty() {
        let mut stderr = std::io::stderr().lock();
        let fatal =
            preflight::report(&findings, &mut stderr).context("writing preflight report")?;
        if fatal {
            return Err(anyhow!(
                "preflight self-test failed — see action items above"
            ));
        }
    }

    let (event_tx, _event_rx) = spawn_bus(config.bus_capacity);
    let shutdown = CancellationToken::new();

    install_signal_handlers(shutdown.clone());

    let profile_store =
        build_profile_store(&config, event_tx.clone()).context("opening profile store")?;

    // Live config snapshot. Wrapped in `Arc<RwLock>` so the
    // ReloadCoordinator can swap it on a successful `ReloadConfig`
    // call and future readers see the new value. Today only the
    // coordinator reads/writes this — backends still receive their
    // section via spawn-time copies — but it's the seam future
    // backend reload hooks will wire onto.
    let live_config: Arc<tokio::sync::RwLock<Config>> =
        Arc::new(tokio::sync::RwLock::new(config.clone()));
    let reload_coordinator = Arc::new(ReloadCoordinator::new(
        args.config_path.clone(),
        Arc::clone(&live_config),
        log_setter,
    ));

    let supervisors = spawn_all(
        &config,
        event_tx.clone(),
        profile_store,
        shutdown.clone(),
        Arc::clone(&reload_coordinator),
    )
    .await
    .context("spawning subsystems")?;

    info!(
        subsystems = supervisors.len(),
        "nexusd up — awaiting shutdown signal"
    );
    // Tell systemd we're ready. When NOTIFY_SOCKET is unset (running
    // outside systemd / tests) this is a no-op. Errors are logged but
    // non-fatal — a broken notify socket shouldn't stop the daemon.
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        warn!(error = ?e, "sd_notify READY failed");
    }

    shutdown.cancelled().await;
    info!("shutdown signalled — joining supervisors");
    // Inform systemd so it marks the unit as stopping rather than
    // still-active during the join window.
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
        warn!(error = ?e, "sd_notify STOPPING failed");
    }
    for (name, join) in supervisors {
        if let Err(e) = join.await {
            warn!(subsystem = name.as_str(), error = ?e, "supervisor join error");
        }
    }

    info!("nexusd exited cleanly");
    Ok(())
}

/// Build the global tracing subscriber and return a callback the
/// reload coordinator uses to swap the active filter.
///
/// We layer a `reload::Layer` over `EnvFilter` so the active
/// directive is mutable at runtime. `RUST_LOG` still wins on first
/// boot (matches the pre-reload behaviour); subsequent
/// `Manager.ReloadConfig` calls update the filter via the handle.
fn init_tracing(level: &str) -> LogLevelSetter {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let initial = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let (filter_layer, handle) = tracing_subscriber::reload::Layer::new(initial);
    // `try_init` so re-running under a test harness that already
    // installed its own subscriber doesn't panic.
    let _ = tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer())
        .try_init();
    Arc::new(move |new_level: &str| -> std::result::Result<(), String> {
        let new_filter = EnvFilter::try_new(new_level)
            .map_err(|e| format!("invalid log_level '{new_level}': {e}"))?;
        handle
            .modify(|f| *f = new_filter)
            .map_err(|e| format!("tracing reload handle dropped: {e}"))
    })
}

fn install_signal_handlers(shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = ?e, "failed to register SIGTERM handler");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = ?e, "failed to register SIGINT handler");
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM");
            }
            _ = sigint.recv() => {
                info!("received SIGINT");
            }
        }
        shutdown.cancel();
    });
}

fn build_profile_store(
    config: &Config,
    event_tx: broadcast::Sender<NexusEvent>,
) -> Result<Arc<dyn ProfileStore>> {
    let source: Box<dyn MasterKeySource> = match config.profile_store.key_source.as_str() {
        "file" => {
            let key_path = config.profile_store.root.join("keys").join("master.key");
            Box::new(FileKeySource::new(key_path))
        }
        "in_memory" => {
            let seed = config
                .profile_store
                .in_memory_seed
                .as_deref()
                .ok_or_else(|| {
                    anyhow!(
                        "profile_store.in_memory_seed is required when key_source = \"in_memory\""
                    )
                })?;
            let bytes = decode_hex_32(seed).context("decoding profile_store.in_memory_seed")?;
            Box::new(InMemoryKeySource::new(bytes))
        }
        other => {
            return Err(anyhow!(
                "profile_store.key_source {other:?} is not supported (use file or in_memory)"
            ));
        }
    };
    let store = ProfileFileStore::open(&config.profile_store.root, source.as_ref())
        .with_context(|| {
            format!(
                "opening profile store at {}",
                config.profile_store.root.display()
            )
        })?
        .with_event_tx(event_tx);
    Ok(Arc::new(store))
}

fn decode_hex_32(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 {
        return Err(anyhow!("expected 64 hex chars, got {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        other => Err(anyhow!("non-hex byte: 0x{other:02x}")),
    }
}

/// Spawn every enabled subsystem and return a vector of
/// `(name, JoinHandle)` pairs. `JoinHandle::<()>` because the
/// supervisor swallows subsystem errors (logs + OperatorNotification).
async fn spawn_all(
    config: &Config,
    event_tx: broadcast::Sender<NexusEvent>,
    profile_store: Arc<dyn ProfileStore>,
    shutdown: CancellationToken,
    reload_coordinator: Arc<ReloadCoordinator>,
) -> Result<Vec<(SubsystemName, tokio::task::JoinHandle<()>)>> {
    let mut out = Vec::new();

    if config.interface_monitor.enabled {
        let ev = event_tx.clone();
        out.push((
            SubsystemName::InterfaceMonitor,
            spawn_supervised(
                SubsystemName::InterfaceMonitor,
                config.supervision.clone(),
                event_tx.clone(),
                shutdown.clone(),
                move |cancel| {
                    let ev = ev.clone();
                    async move {
                        let join = spawn_interface_monitor(ev, cancel).await?;
                        let res = join.await?;
                        res.map_err(|e| anyhow!("{e}"))
                    }
                },
            ),
        ));
    } else {
        info!("interface_monitor disabled — skipping");
    }

    if config.ethernet.enabled {
        let ev = event_tx.clone();
        let store = Arc::clone(&profile_store);
        let eth_cfg = build_ethernet_config(&config.ethernet)?;
        out.push((
            SubsystemName::Ethernet,
            spawn_supervised(
                SubsystemName::Ethernet,
                config.supervision.clone(),
                event_tx.clone(),
                shutdown.clone(),
                move |cancel| {
                    let ev = ev.clone();
                    let store = Arc::clone(&store);
                    async move {
                        // Wired 802.1X is pluggable; for now we spawn the
                        // backend without one. When the wpa_supplicant /
                        // ead WiredAuthBackend lands, config selects it.
                        let join = spawn_ethernet_backend(ev, store, None, eth_cfg, cancel)
                            .await
                            .map_err(|e| anyhow!("{e}"))?;
                        let res = join.await?;
                        res.map_err(|e| anyhow!("{e}"))
                    }
                },
            ),
        ));
    } else {
        info!("ethernet disabled — skipping");
    }

    // Create the wifi command channel at the daemon level so the
    // D-Bus `BackendOps` layer (spawned below in the `dbus` arm) can
    // hold a clone of the sender while the supervised wifi task
    // owns the receiver. `Option` so the dbus arm can tell whether
    // wifi is live and swap in `WifiBackendOps` accordingly.
    let mut wifi_cmd_tx: Option<tokio::sync::mpsc::Sender<nexus_wifi::WifiCommand>> = None;
    if config.wifi.enabled {
        let ev = event_tx.clone();
        let store = Arc::clone(&profile_store);
        let wifi_cfg = build_wifi_config(&config.wifi)?;
        let backend_kind = config.wifi.backend.clone();
        let supplicant_cap = config.wifi.supplicant_event_capacity;
        let (cmd_tx, cmd_rx) = nexus_wifi::command_channel();
        wifi_cmd_tx = Some(cmd_tx);
        // `cmd_rx` is !Clone — wrap in an Option so the FnMut
        // closure can `take()` it on first call. The supervisor
        // only invokes the closure once per lifecycle start.
        let cmd_rx = Arc::new(tokio::sync::Mutex::new(Some(cmd_rx)));
        out.push((
            SubsystemName::Wifi,
            spawn_supervised(
                SubsystemName::Wifi,
                config.supervision.clone(),
                event_tx.clone(),
                shutdown.clone(),
                move |cancel| {
                    let ev = ev.clone();
                    let store = Arc::clone(&store);
                    let backend_kind = backend_kind.clone();
                    let cmd_rx = Arc::clone(&cmd_rx);
                    async move {
                        let (sup_tx, _sup_rx) = broadcast::channel(supplicant_cap);
                        let supplicant: Box<dyn WifiSupplicantBackend> =
                            build_supplicant(&backend_kind, sup_tx.clone()).await?;
                        let cmd_rx = cmd_rx
                            .lock()
                            .await
                            .take()
                            .ok_or_else(|| anyhow!("wifi cmd_rx already consumed"))?;
                        let handle = nexus_wifi::spawn_wifi_backend(
                            ev, sup_tx, supplicant, store, wifi_cfg, cmd_rx,
                        );
                        let mut join = handle.join;
                        let inner = handle.shutdown;
                        let res = tokio::select! {
                            r = &mut join => r?,
                            _ = cancel.cancelled() => {
                                inner.cancel();
                                join.await?
                            }
                        };
                        res.map_err(|e| anyhow!("{e}"))
                    }
                },
            ),
        ));
    } else {
        info!("wifi disabled — skipping");
    }

    if config.bluetooth.enabled {
        let ev = event_tx.clone();
        let store = Arc::clone(&profile_store);
        let bt_cfg = build_bluetooth_config(&config.bluetooth);
        let mock = config.bluetooth.mock;
        out.push((
            SubsystemName::Bluetooth,
            spawn_supervised(
                SubsystemName::Bluetooth,
                config.supervision.clone(),
                event_tx.clone(),
                shutdown.clone(),
                move |cancel| {
                    let ev = ev.clone();
                    let store = Arc::clone(&store);
                    let bt_cfg = bt_cfg.clone();
                    async move {
                        let bluez: Arc<dyn nexus_bluetooth::BluezClient> = if mock {
                            Arc::new(MockBluezClient::new(ev.clone()))
                        } else {
                            Arc::new(ZbusBluezClient::new(ev.clone()))
                        };
                        let handle = spawn_bluetooth_backend(bluez, store, ev, bt_cfg);
                        let mut join = handle.join;
                        let inner = handle.shutdown;
                        let res = tokio::select! {
                            r = &mut join => r?,
                            _ = cancel.cancelled() => {
                                inner.cancel();
                                join.await?
                            }
                        };
                        res.map_err(|e| anyhow!("{e}"))
                    }
                },
            ),
        ));
    } else {
        info!("bluetooth disabled — skipping");
    }

    if config.gnss.enabled {
        let ev = event_tx.clone();
        let store = Arc::clone(&profile_store);
        let gnss_cfg = build_gnss_config(&config.gnss);
        let mock = config.gnss.mock;
        let endpoint = config.gnss.gpsd_endpoint;
        out.push((
            SubsystemName::Gnss,
            spawn_supervised(
                SubsystemName::Gnss,
                config.supervision.clone(),
                event_tx.clone(),
                shutdown.clone(),
                move |cancel| {
                    let ev = ev.clone();
                    let store = Arc::clone(&store);
                    let gnss_cfg = gnss_cfg.clone();
                    async move {
                        let client: Arc<dyn nexus_gnss::GpsdClient> = if mock {
                            Arc::new(MockGpsdClient::new(ev.clone()))
                        } else {
                            Arc::new(JsonGpsdClient::new(endpoint, ev.clone()))
                        };
                        let handle = spawn_gnss_backend(client, store, ev, gnss_cfg);
                        let mut join = handle.join;
                        let inner = handle.shutdown;
                        let res = tokio::select! {
                            r = &mut join => r?,
                            _ = cancel.cancelled() => {
                                inner.cancel();
                                join.await?
                            }
                        };
                        res.map_err(|e| anyhow!("{e}"))
                    }
                },
            ),
        ));
    } else {
        info!("gnss disabled — skipping");
    }

    if config.dbus.enabled {
        let store = Arc::clone(&profile_store);
        let dbus_cfg =
            build_dbus_config(config, Arc::clone(&reload_coordinator), wifi_cmd_tx.clone()).await?;
        // Subscribe now, before any event-producing subsystem starts
        // (interface_monitor's cold-boot dump fires during bootstrap).
        // If we subscribed inside the supervised closure, the dbus
        // subsystem would race against that dump and miss every
        // InterfaceDiscovered event — the daemon would own
        // `/fi/nexus1` but register no per-interface objects.
        let dbus_event_rx = event_tx.subscribe();
        // Same `Arc<Mutex<Option<_>>>` trick as the wifi cmd_rx: the
        // supervisor closure is FnMut; stash the !Clone receiver
        // behind a `take()` on first start.
        let dbus_event_rx = Arc::new(tokio::sync::Mutex::new(Some(dbus_event_rx)));
        out.push((
            SubsystemName::Dbus,
            spawn_supervised(
                SubsystemName::Dbus,
                config.supervision.clone(),
                event_tx.clone(),
                shutdown.clone(),
                move |cancel| {
                    let store = Arc::clone(&store);
                    let dbus_cfg = dbus_cfg.clone();
                    let rx_slot = Arc::clone(&dbus_event_rx);
                    async move {
                        let rx = rx_slot
                            .lock()
                            .await
                            .take()
                            .ok_or_else(|| anyhow!("dbus event_rx already consumed"))?;
                        let handle = spawn_dbus_service(rx, store, dbus_cfg)
                            .await
                            .map_err(|e| anyhow!("{e}"))?;
                        let mut join = handle.join;
                        let inner = handle.shutdown;
                        let res = tokio::select! {
                            r = &mut join => r?,
                            _ = cancel.cancelled() => {
                                inner.cancel();
                                join.await?
                            }
                        };
                        res.map_err(|e| anyhow!("{e}"))
                    }
                },
            ),
        ));
    } else {
        info!("dbus disabled — skipping");
    }

    Ok(out)
}

fn build_ethernet_config(section: &nexus_daemon::EthernetSection) -> Result<EthernetConfig> {
    let kind = match section.auth_backend.as_str() {
        "wpa_supplicant" => AuthBackendKind::WpaSupplicant,
        "ead" => AuthBackendKind::Ead,
        "none" => AuthBackendKind::None,
        other => return Err(anyhow!("ethernet.auth_backend {other:?} not supported")),
    };
    Ok(EthernetConfig {
        auth_backend: kind,
        retry: RetryPolicy {
            initial: section.retry_initial,
            max: section.retry_max,
            multiplier: section.retry_multiplier,
            max_attempts: section.retry_max_attempts,
        },
    })
}

fn build_wifi_config(section: &nexus_daemon::WifiSection) -> Result<WifiConfig> {
    use nexus_wifi::types::RoamMode;
    let roam_mode = match section.roam_mode.as_str() {
        "off" => RoamMode::Off,
        "supplicant" => RoamMode::Supplicant,
        "nexus" => RoamMode::Nexus,
        other => return Err(anyhow!("wifi.roam_mode {other:?} not supported")),
    };
    Ok(WifiConfig {
        roam_mode,
        signal_poll_interval: section.signal_poll_interval,
        disconnect_cool_down: section.disconnect_cool_down,
        ..WifiConfig::default()
    })
}

async fn build_supplicant(
    kind: &str,
    sup_tx: broadcast::Sender<nexus_wifi::supplicant::SupplicantEvent>,
) -> Result<Box<dyn WifiSupplicantBackend>> {
    match kind {
        "wpa_supplicant" => {
            let sb = nexus_wifi::supplicant::wpa_supplicant::WpaSupplicantBackend::new(sup_tx)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            Ok(Box::new(sb))
        }
        "mock" => Ok(Box::new(MockSupplicant::new(sup_tx))),
        "iwd" => Err(anyhow!(
            "wifi.backend = \"iwd\" is recognized but the iwd supplicant impl is not yet available"
        )),
        other => Err(anyhow!("wifi.backend {other:?} not supported")),
    }
}

fn build_bluetooth_config(section: &nexus_daemon::BluetoothSection) -> BluetoothConfig {
    BluetoothConfig {
        pairing_timeout_s: section.pairing_timeout_s,
        agent_response_timeout_s: section.agent_response_timeout_s,
        discovery_timeout_s: section.discovery_timeout_s,
        discovery_device_ttl_s: section.discovery_device_ttl_s,
        bluez_outage_notify_s: section.bluez_outage_notify_s,
        auto_power_on_startup: section.auto_power_on_startup,
        register_agent: section.register_agent,
        ..BluetoothConfig::default()
    }
}

fn build_gnss_config(section: &nexus_daemon::GnssSection) -> GnssConfig {
    GnssConfig {
        gpsd_endpoint: section.gpsd_endpoint,
        acquisition_timeout_s: section.acquisition_timeout_s,
        tpv_stall_timeout_s: section.tpv_stall_timeout_s,
        gpsd_outage_notify_s: section.gpsd_outage_notify_s,
        defaults: GnssDefaults::default(),
    }
}

async fn build_dbus_config(
    config: &Config,
    reload_coordinator: Arc<ReloadCoordinator>,
    wifi_commands: Option<tokio::sync::mpsc::Sender<nexus_wifi::WifiCommand>>,
) -> Result<DbusConfig> {
    let auth = if config.dbus.allow_all_authz {
        always_allow()
    } else {
        // Open a separate connection to the system bus for PolicyKit
        // calls. Keeping it distinct from the one `spawn_dbus_service`
        // opens avoids tangling the lifecycle of the PolicyKit proxy
        // with the service registration flow.
        let conn = if let Some(addr) = config.dbus.address.as_deref() {
            zbus::connection::Builder::address(addr)?.build().await?
        } else if config.dbus.use_session_bus {
            zbus::Connection::session().await?
        } else {
            zbus::Connection::system().await?
        };
        Arc::new(PolicyKitChecker::new(conn)) as Arc<dyn nexus_dbus::AuthChecker>
    };
    let rate_limits = RateLimits {
        property_read_per_min: config.dbus.rate_limit_property_read_per_min,
        scan_per_min: config.dbus.rate_limit_scan_per_min,
        connect_disconnect_per_min: config.dbus.rate_limit_connect_per_min,
        profile_write_per_min: config.dbus.rate_limit_profile_write_per_min,
        admin_per_min: config.dbus.rate_limit_admin_per_min,
    };
    // BackendOps layering, bottom → top:
    //   NoopOps              — every method returns Unsupported by default.
    //   WifiBackendOps       — overrides wifi_scan; only present when the
    //                          wifi subsystem is enabled.
    //   ReloadOps            — always on top; routes Manager.ReloadConfig
    //                          into the daemon's ReloadCoordinator and
    //                          passes every other method through.
    let noop: Arc<dyn BackendOps> = NoopOps::arc();
    let mid: Arc<dyn BackendOps> = match wifi_commands {
        Some(tx) => WifiBackendOps::new(tx, noop),
        None => noop,
    };
    let ops: Arc<dyn BackendOps> = ReloadOps::new(reload_coordinator, mid);
    Ok(DbusConfig {
        bus_name: config.dbus.bus_name.clone(),
        use_session_bus: config.dbus.use_session_bus,
        address: config.dbus.address.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        auth,
        ops,
        rate_limits,
        enabled_features: EnabledFeatures {
            ethernet: config.ethernet.enabled,
            wifi: config.wifi.enabled,
            bluetooth: config.bluetooth.enabled,
            gnss: config.gnss.enabled,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap_run(parsed: ParsedArgs) -> CliArgs {
        match parsed {
            ParsedArgs::Run(a) => a,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_default() {
        let args = unwrap_run(parse_args(vec![]).unwrap());
        assert_eq!(args.config_path, PathBuf::from("/etc/nexus/nexus.toml"));
    }

    #[test]
    fn parse_args_with_config() {
        let args = unwrap_run(parse_args(vec!["--config".into(), "/tmp/foo.toml".into()]).unwrap());
        assert_eq!(args.config_path, PathBuf::from("/tmp/foo.toml"));
    }

    #[test]
    fn parse_args_missing_value() {
        assert!(parse_args(vec!["--config".into()]).is_err());
    }

    #[test]
    fn parse_args_unknown() {
        assert!(parse_args(vec!["--wat".into()]).is_err());
    }

    #[test]
    fn parse_args_help_long_and_short() {
        assert_eq!(
            parse_args(vec!["--help".into()]).unwrap(),
            ParsedArgs::PrintHelp
        );
        assert_eq!(
            parse_args(vec!["-h".into()]).unwrap(),
            ParsedArgs::PrintHelp
        );
    }

    #[test]
    fn parse_args_version_long_and_short() {
        assert_eq!(
            parse_args(vec!["--version".into()]).unwrap(),
            ParsedArgs::PrintVersion
        );
        assert_eq!(
            parse_args(vec!["-V".into()]).unwrap(),
            ParsedArgs::PrintVersion
        );
    }

    #[test]
    fn parse_args_help_wins_over_config() {
        // `--help` short-circuits, matching clap's behaviour. Lets
        // operators run `nexusd --config /nonexistent --help` without
        // the nonexistent config producing an error.
        assert_eq!(
            parse_args(vec![
                "--config".into(),
                "/tmp/x.toml".into(),
                "--help".into()
            ])
            .unwrap(),
            ParsedArgs::PrintHelp
        );
    }

    #[test]
    fn help_text_mentions_key_topics() {
        // The message is part of the operator-visible surface. If any
        // of these topics stop appearing in --help, we've regressed.
        let h = help_text();
        assert!(h.contains("--config"));
        assert!(h.contains("--help"));
        assert!(h.contains("--version"));
        assert!(h.contains("RUST_LOG"));
        assert!(h.contains("SIGTERM"));
        assert!(h.contains("NOTIFY_SOCKET"));
        // The self-test section is an operator-visible contract.
        assert!(h.contains("Self-test"));
        // Current package version is baked in.
        assert!(h.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn decode_hex_round_trip() {
        let bytes =
            decode_hex_32("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")
                .unwrap();
        assert_eq!(bytes[0], 0x00);
        assert_eq!(bytes[1], 0x11);
        assert_eq!(bytes[31], 0xff);
    }

    #[test]
    fn decode_hex_wrong_length() {
        assert!(decode_hex_32("abcd").is_err());
    }

    #[test]
    fn decode_hex_bad_char() {
        let s = "zz".repeat(32);
        assert!(decode_hex_32(&s).is_err());
    }
}
