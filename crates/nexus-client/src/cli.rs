//! Public CLI shape, defined with clap's derive API. DD-008 §4.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::output::{ColorChoice, OutputFormat, RenderContext};

/// `nexusctl` — the Nexus command-line client.
#[derive(Debug, Parser)]
#[command(
    name = "nexusctl",
    version,
    about = "Command-line client for nexusd",
    long_about = None,
    infer_subcommands = true,
    subcommand_required = false,
    arg_required_else_help = false,
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Global options per DD-008 §4.2.
#[derive(Debug, Args, Default, Clone)]
pub struct GlobalOpts {
    #[arg(global = true, long, short = 'f', value_enum)]
    pub format: Option<OutputFormat>,

    #[arg(
        global = true,
        long,
        conflicts_with_all = ["format", "terse", "pretty"]
    )]
    pub json: bool,

    #[arg(
        global = true,
        long,
        short = 't',
        conflicts_with_all = ["format", "json", "pretty"]
    )]
    pub terse: bool,

    #[arg(
        global = true,
        long,
        conflicts_with_all = ["format", "json", "terse"]
    )]
    pub pretty: bool,

    #[arg(global = true, long, value_delimiter = ',')]
    pub fields: Option<Vec<String>>,

    #[arg(global = true, long, default_value = ":")]
    pub separator: String,

    #[arg(global = true, long, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    #[arg(global = true, long)]
    pub no_color: bool,

    #[arg(global = true, long, value_name = "SECONDS")]
    pub timeout: Option<u64>,

    #[arg(global = true, long, value_name = "ADDRESS")]
    pub bus: Option<String>,

    #[arg(global = true, long, short = 'v')]
    pub verbose: bool,

    #[arg(global = true, long, short = 'q')]
    pub quiet: bool,

    #[arg(global = true, long)]
    pub no_interactive: bool,

    /// Skip auto-spawning `pkttyagent` for PolicyKit prompts
    /// (DD-008 §6.3). Useful when the caller has already got an
    /// agent or explicitly wants mutating commands to fail hard
    /// on AuthFailed.
    #[arg(global = true, long)]
    pub no_polkit_agent: bool,

    #[arg(global = true, long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

impl GlobalOpts {
    pub fn output_format(&self) -> OutputFormat {
        if self.json {
            return OutputFormat::Json;
        }
        if self.terse {
            return OutputFormat::Terse;
        }
        if self.pretty {
            return OutputFormat::Pretty;
        }
        if let Some(f) = self.format {
            return f;
        }
        if let Ok(raw) = std::env::var("NEXUSCTL_FORMAT") {
            return match raw.as_str() {
                "human" => OutputFormat::Human,
                "terse" => OutputFormat::Terse,
                "json" => OutputFormat::Json,
                "pretty" => OutputFormat::Pretty,
                _ => OutputFormat::Human,
            };
        }
        OutputFormat::Human
    }

    pub fn color_choice(&self) -> ColorChoice {
        if self.no_color {
            ColorChoice::Never
        } else {
            self.color
        }
    }

    pub fn render_context(&self) -> RenderContext {
        RenderContext {
            fields: self.fields.clone(),
            separator: self.separator.clone(),
            color: self.color_choice(),
        }
    }
}

/// DD-008 §4.1 `--kind` values for list filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum InterfaceKind {
    Ethernet,
    Wifi,
    Bluetooth,
    Gnss,
}

impl InterfaceKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            InterfaceKind::Ethernet => "ethernet",
            InterfaceKind::Wifi => "wifi",
            InterfaceKind::Bluetooth => "bluetooth",
            InterfaceKind::Gnss => "gnss",
        }
    }
}

/// DD-008 §4.1 `--kind` for profile filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ProfileKind {
    Ethernet,
    Wifi,
}

impl ProfileKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            ProfileKind::Ethernet => "ethernet",
            ProfileKind::Wifi => "wifi",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show overall daemon status.
    Status,
    /// Interface operations.
    Iface {
        #[command(subcommand)]
        sub: IfaceSub,
    },
    /// Ethernet shortcuts (equivalent to `iface … --kind ethernet`).
    Eth {
        #[command(subcommand)]
        sub: EthSub,
    },
    /// Wi-Fi operations (read-only subset; `scan`/`connect` land in
    /// Phase 7.4).
    Wifi {
        #[command(subcommand)]
        sub: WifiSub,
    },
    /// Bluetooth operations.
    Bt {
        #[command(subcommand)]
        sub: BtSub,
    },
    /// GNSS operations.
    Gnss {
        #[command(subcommand)]
        sub: GnssSub,
    },
    /// Profile management.
    Profile {
        #[command(subcommand)]
        sub: ProfileSub,
    },
    /// PowerState control. Read-only today.
    Power {
        #[command(subcommand)]
        sub: PowerSub,
    },
    /// Administrative operations (read-only subset).
    Admin {
        #[command(subcommand)]
        sub: AdminSub,
    },
    /// Subscribe to D-Bus signals. DD-008 §7.4.
    Watch {
        #[command(subcommand)]
        sub: Option<WatchSub>,
        /// `field=glob`. Repeat for AND semantics.
        #[arg(long = "filter", value_name = "FILTER", global = true)]
        filter: Vec<String>,
    },
}

#[derive(Debug, Subcommand, Clone)]
pub enum WatchSub {
    Events,
    Iface,
    Wifi,
    Bt,
    Gnss,
}

#[derive(Debug, Subcommand)]
pub enum IfaceSub {
    List {
        /// Restrict to a single kind.
        #[arg(long, value_enum)]
        kind: Option<InterfaceKind>,
    },
    Show {
        iface: String,
    },
    /// Last N events for an interface. Planned feature — see
    /// DD-008 §4.1 (the daemon doesn't yet retain history).
    Events {
        iface: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum EthSub {
    List,
    Show { iface: String },
}

#[derive(Debug, Subcommand)]
pub enum WifiSub {
    List,
    Show {
        iface: Option<String>,
    },
    /// Trigger a scan and print results.
    Scan {
        iface: Option<String>,
    },
    /// Connect by SSID. When `--psk` is provided and no matching
    /// profile exists, nexusctl creates one on-the-fly.
    Connect {
        ssid: String,
        #[arg(long)]
        iface: Option<String>,
        #[arg(long)]
        psk: Option<String>,
        /// Suppress the DD-008 §6.2 credential-leak warning.
        #[arg(long)]
        no_warn_psk: bool,
    },
    /// Connect using an existing profile, looked up by ULID or label.
    ConnectProfile {
        profile: String,
        #[arg(long)]
        iface: Option<String>,
    },
    Disconnect {
        iface: Option<String>,
    },
    /// Delete a stored Wi-Fi profile (matched by SSID or ULID).
    Forget {
        reference: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum BtSub {
    Adapters,
    List {
        #[arg(long, conflicts_with = "connected")]
        paired: bool,
        #[arg(long, conflicts_with = "paired")]
        connected: bool,
    },
    Show {
        address: String,
    },
    /// Power an adapter on or off.
    Power {
        hci: String,
        state: OnOff,
    },
    /// Discover devices for a bounded duration.
    Scan {
        hci: Option<String>,
        /// Scan duration in seconds. Defaults to 10.
        #[arg(long, default_value_t = 10)]
        duration: u64,
    },
    Connect {
        address: String,
    },
    Disconnect {
        address: String,
    },
    Forget {
        address: String,
    },
    /// Set or clear the trusted flag.
    Trust {
        address: String,
        state: OnOff,
    },
    /// Interactive pairing. DD-008 §6.1.
    Pair {
        address: String,
        /// Override `bluetooth.agent_response_timeout_s` for this
        /// invocation.
        #[arg(long, default_value_t = 90)]
        timeout: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum OnOff {
    On,
    Off,
}

impl OnOff {
    pub fn as_bool(self) -> bool {
        matches!(self, OnOff::On)
    }
}

#[derive(Debug, Subcommand)]
pub enum GnssSub {
    List,
    Show { device: Option<String> },
    Satellites { device: Option<String> },
}

#[derive(Debug, Subcommand)]
pub enum ProfileSub {
    List {
        #[arg(long, value_enum)]
        kind: Option<ProfileKind>,
    },
    Show {
        reference: String,
    },
    Export {
        reference: String,
    },
    /// Create a Wi-Fi profile. Either supply `<ssid>` inline along
    /// with `--psk`, or `--file <path>` / `--file -` for a full TOML.
    AddWifi {
        /// SSID (required unless `--file` is given).
        ssid: Option<String>,
        #[arg(long)]
        psk: Option<String>,
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        priority: Option<i32>,
        #[arg(long)]
        auto_connect: Option<bool>,
        #[arg(long)]
        hidden: Option<bool>,
        #[arg(long)]
        fast_transition: Option<bool>,
        /// Security type. Defaults to `wpa2_personal` when `--psk`
        /// is supplied, `open` when no PSK is given.
        #[arg(long, default_value = "auto")]
        security: String,
        /// Suppress the --psk leak warning.
        #[arg(long)]
        no_warn_psk: bool,
    },
    /// Create an Ethernet profile.
    AddEthernet {
        ifname: Option<String>,
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        auto_connect: Option<bool>,
    },
    /// Read a TOML profile from stdin (or `--file`).
    Import {
        #[arg(long, value_enum)]
        kind: Option<ProfileKind>,
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
    },
    /// Remove a stored profile.
    Remove {
        reference: String,
    },
    /// Update a single top-level field on a profile.
    Update {
        reference: String,
        /// Field name as it appears in the profile's settings dict
        /// (e.g., `label`, `auto_connect`).
        #[arg(long)]
        field: String,
        /// Literal string value; parsed against the field's
        /// expected type.
        #[arg(long)]
        value: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum PowerSub {
    Get,
    /// Change the daemon's power state.
    Set {
        #[arg(value_enum)]
        state: PowerStateArg,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum PowerStateArg {
    Active,
    Background,
    Sleep,
}

impl PowerStateArg {
    pub fn as_wire(self) -> &'static str {
        match self {
            PowerStateArg::Active => "active",
            PowerStateArg::Background => "background",
            PowerStateArg::Sleep => "sleep",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AdminSub {
    MasterKeyInfo,
    /// Fire-and-forget rotation — prints the job id for
    /// `nexusctl watch`.
    RotateMasterKey,
    /// Acquire a backup lease (prints the lease token).
    FreezeBackup,
    /// Release a previously-acquired lease.
    ReleaseBackup {
        lease: String,
    },
    /// Request a support bundle. Phase 7.4 stub — streaming the
    /// bundle fd lands in a later phase.
    Diagnostics {
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },
    /// Re-read nexus.toml and apply reloadable fields.
    ReloadConfig,
}
