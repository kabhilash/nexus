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
    /// Output format: human, terse, json, or pretty.
    #[arg(global = true, long, short = 'f', value_enum)]
    pub format: Option<OutputFormat>,

    /// Shorthand for `--format json`.
    #[arg(
        global = true,
        long,
        conflicts_with_all = ["format", "terse", "pretty"]
    )]
    pub json: bool,

    /// Shorthand for `--format terse` (separator-joined, one record
    /// per line — ideal for shell scripts).
    #[arg(
        global = true,
        long,
        short = 't',
        conflicts_with_all = ["format", "json", "pretty"]
    )]
    pub terse: bool,

    /// Shorthand for `--format pretty` (verbose single-record view).
    #[arg(
        global = true,
        long,
        conflicts_with_all = ["format", "json", "terse"]
    )]
    pub pretty: bool,

    /// Comma-separated field whitelist for `--terse` output.
    #[arg(global = true, long, value_delimiter = ',')]
    pub fields: Option<Vec<String>>,

    /// Separator for `--terse` output. Default `:`.
    #[arg(global = true, long, default_value = ":")]
    pub separator: String,

    /// When to use ANSI colors.
    #[arg(global = true, long, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Disable ANSI colors (equivalent to `--color never`).
    #[arg(global = true, long)]
    pub no_color: bool,

    /// Override the D-Bus call timeout for this invocation (seconds).
    #[arg(global = true, long, value_name = "SECONDS")]
    pub timeout: Option<u64>,

    /// D-Bus bus address (for test harnesses running on session bus).
    #[arg(global = true, long, value_name = "ADDRESS")]
    pub bus: Option<String>,

    /// Log D-Bus calls and signal subscriptions to stderr.
    #[arg(global = true, long, short = 'v')]
    pub verbose: bool,

    /// Suppress non-essential output.
    #[arg(global = true, long, short = 'q')]
    pub quiet: bool,

    /// Fail instead of prompting. Default: auto-detect TTY.
    #[arg(global = true, long)]
    pub no_interactive: bool,

    /// Skip auto-spawning `pkttyagent` for PolicyKit prompts. Useful
    /// when the caller has already got an agent, or explicitly wants
    /// mutating commands to fail hard on AuthFailed.
    #[arg(global = true, long)]
    pub no_polkit_agent: bool,

    /// Alternative config file path.
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
    #[command(long_about = "Print the daemon version, power state, and \
        a one-line summary per managed interface.\n\n\
        Example:\n    \
        nexusctl status")]
    Status,
    /// Interface operations.
    #[command(long_about = "List, inspect, and follow managed network \
        interfaces.\n\n\
        Examples:\n    \
        nexusctl iface list\n    \
        nexusctl iface list --kind wifi\n    \
        nexusctl iface show eth0")]
    Iface {
        #[command(subcommand)]
        sub: IfaceSub,
    },
    /// Ethernet shortcuts (equivalent to `iface ... --kind ethernet`).
    #[command(long_about = "Convenience aliases for Ethernet \
        interfaces. Equivalent to `iface list --kind ethernet` / \
        `iface show <iface>` but keeps the verbs grouped by \
        technology.\n\n\
        Examples:\n    \
        nexusctl eth list\n    \
        nexusctl eth show eth0")]
    Eth {
        #[command(subcommand)]
        sub: EthSub,
    },
    /// Wi-Fi operations.
    #[command(long_about = "List Wi-Fi interfaces, scan, connect, and \
        manage stored profiles.\n\n\
        Examples:\n    \
        nexusctl wifi list\n    \
        nexusctl wifi scan wlan0\n    \
        nexusctl wifi connect 'home-ssid' --psk 'secret'\n    \
        nexusctl wifi disconnect wlan0")]
    Wifi {
        #[command(subcommand)]
        sub: WifiSub,
    },
    /// Bluetooth operations.
    #[command(long_about = "List adapters and devices, pair, connect, \
        and manage trust.\n\n\
        Examples:\n    \
        nexusctl bt adapters\n    \
        nexusctl bt scan hci0 --duration 15\n    \
        nexusctl bt pair AA:BB:CC:DD:EE:01\n    \
        nexusctl bt connect AA:BB:CC:DD:EE:01")]
    Bt {
        #[command(subcommand)]
        sub: BtSub,
    },
    /// GNSS operations.
    #[command(long_about = "Inspect GNSS devices and their current \
        fix.\n\n\
        Examples:\n    \
        nexusctl gnss list\n    \
        nexusctl gnss show /dev/gps0\n    \
        nexusctl gnss satellites")]
    Gnss {
        #[command(subcommand)]
        sub: GnssSub,
    },
    /// Profile management.
    #[command(long_about = "Create, list, update, import, export, and \
        delete stored Wi-Fi / Ethernet profiles.\n\n\
        Examples:\n    \
        nexusctl profile list\n    \
        nexusctl profile show home-ssid\n    \
        nexusctl profile add-wifi 'home-ssid' --psk 'secret' --label home\n    \
        nexusctl profile remove home-ssid\n    \
        nexusctl profile export home-ssid")]
    Profile {
        #[command(subcommand)]
        sub: ProfileSub,
    },
    /// PowerState control.
    #[command(long_about = "Inspect or change the daemon's PowerState \
        (active / background / sleep).\n\n\
        Examples:\n    \
        nexusctl power get\n    \
        nexusctl power set background")]
    Power {
        #[command(subcommand)]
        sub: PowerSub,
    },
    /// Administrative operations.
    #[command(long_about = "Master-key rotation, backup lease, \
        diagnostics bundle, and config reload. All require \
        PolicyKit authorisation.\n\n\
        Examples:\n    \
        nexusctl admin master-key-info\n    \
        nexusctl admin rotate-master-key\n    \
        nexusctl admin reload-config")]
    Admin {
        #[command(subcommand)]
        sub: AdminSub,
    },
    /// Subscribe to D-Bus signals and print a live event stream.
    #[command(long_about = "Stream events from nexusd — interface \
        arrivals, link-state changes, Wi-Fi scans, Bluetooth \
        pairing steps, and more. Use `--filter 'field=glob'` to \
        narrow; repeat the flag for AND semantics.\n\n\
        Examples:\n    \
        nexusctl watch\n    \
        nexusctl watch wifi\n    \
        nexusctl watch --filter 'iface=wlan0' --filter 'kind=wifi-*'")]
    Watch {
        #[command(subcommand)]
        sub: Option<WatchSub>,
        /// `field=glob`. Repeat for AND semantics.
        #[arg(long = "filter", value_name = "FILTER", global = true)]
        filter: Vec<String>,
    },
    /// Emit a shell completion script on stdout.
    #[command(long_about = "Emit a shell completion script on \
        stdout. Redirect into the packager-appropriate path; \
        nexusctl does not install the file itself. See DD-008 §10 \
        for the full install matrix.\n\n\
        Examples:\n    \
        nexusctl completions bash > /etc/bash_completion.d/nexusctl\n    \
        nexusctl completions zsh  > ~/.zfunc/_nexusctl\n    \
        nexusctl completions fish > ~/.config/fish/completions/nexusctl.fish")]
    Completions {
        /// Target shell. One of: bash, zsh, fish, powershell (pwsh).
        #[arg(value_enum)]
        shell: CompletionShell,
    },
}

/// Shells `nexusctl completions` can emit for. DD-008 §10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    #[value(alias = "pwsh")]
    Powershell,
}

impl CompletionShell {
    pub fn as_completion_shell(self) -> crate::completion::Shell {
        match self {
            CompletionShell::Bash => crate::completion::Shell::Bash,
            CompletionShell::Zsh => crate::completion::Shell::Zsh,
            CompletionShell::Fish => crate::completion::Shell::Fish,
            CompletionShell::Powershell => crate::completion::Shell::PowerShell,
        }
    }
}

#[derive(Debug, Subcommand, Clone)]
pub enum WatchSub {
    /// All events (the default).
    Events,
    /// Interface arrivals, departures, and link-state changes.
    Iface,
    /// Wi-Fi scans, state transitions, signal updates.
    Wifi,
    /// Bluetooth adapter/device state and pairing events.
    Bt,
    /// GNSS fix updates.
    Gnss,
}

#[derive(Debug, Subcommand)]
pub enum IfaceSub {
    /// List managed interfaces.
    List {
        /// Restrict to a single kind.
        #[arg(long, value_enum)]
        kind: Option<InterfaceKind>,
    },
    /// Show detailed state for a single interface.
    Show { iface: String },
    /// Last N events for an interface. Planned feature — the daemon
    /// doesn't yet retain history.
    Events { iface: String },
}

#[derive(Debug, Subcommand)]
pub enum EthSub {
    /// List Ethernet interfaces.
    List,
    /// Show detailed Ethernet state, including 802.1X authentication.
    Show { iface: String },
}

#[derive(Debug, Subcommand)]
pub enum WifiSub {
    /// List Wi-Fi interfaces.
    List,
    /// Show the current connection, signal, and security.
    Show { iface: Option<String> },
    /// Trigger a scan and print results.
    Scan { iface: Option<String> },
    /// Connect by SSID. When `--psk` is provided and no matching
    /// profile exists, nexusctl creates one on-the-fly.
    Connect {
        ssid: String,
        #[arg(long)]
        iface: Option<String>,
        #[arg(long)]
        psk: Option<String>,
        /// Suppress the credential-leak warning.
        #[arg(long)]
        no_warn_psk: bool,
    },
    /// Connect using an existing profile, looked up by ULID,
    /// label, or SSID. Resolution prefers ULID → label → SSID in
    /// that order; ambiguous matches (two profiles for the same
    /// label or SSID) surface as a usage error.
    ConnectProfile {
        profile: String,
        #[arg(long)]
        iface: Option<String>,
    },
    /// Disconnect the current Wi-Fi session.
    ///
    /// `--pause-auto-connect` also blocks the active profile from
    /// the daemon's auto-connect picker until daemon restart, an
    /// explicit `wifi connect`, or a profile edit. Runtime-only —
    /// the profile file's `auto_connect` field is not modified.
    Disconnect {
        iface: Option<String>,
        /// Block the active profile from auto-connect for this
        /// session without touching the on-disk profile.
        #[arg(long = "pause-auto-connect")]
        pause_auto_connect: bool,
    },
    /// Toggle soft-rfkill for a Wi-Fi interface (the
    /// `fi.nexus.Wifi.Powered` setter). `on` releases the soft
    /// block; `off` asserts it. With multiple Wi-Fi interfaces
    /// present, pass `iface` explicitly; otherwise nexusctl
    /// auto-selects the only one.
    Power {
        state: OnOff,
        iface: Option<String>,
    },
    /// Delete a stored Wi-Fi profile (matched by SSID or ULID).
    Forget { reference: String },
    /// List saved Wi-Fi profiles with SSID, security, priority,
    /// auto-connect, and credential status. The kind-agnostic
    /// `nexusctl profile list --kind wifi` returns a generic row;
    /// this subcommand returns Wi-Fi-specific columns.
    Profiles,
}

#[derive(Debug, Subcommand)]
pub enum BtSub {
    /// List Bluetooth adapters.
    Adapters,
    /// List known Bluetooth devices.
    List {
        #[arg(long, conflicts_with = "connected")]
        paired: bool,
        #[arg(long, conflicts_with = "paired")]
        connected: bool,
    },
    /// Show detailed state for a Bluetooth device.
    Show { address: String },
    /// Power an adapter on or off.
    Power { hci: String, state: OnOff },
    /// Discover devices for a bounded duration.
    Scan {
        hci: Option<String>,
        /// Scan duration in seconds. Defaults to 10.
        #[arg(long, default_value_t = 10)]
        duration: u64,
    },
    /// Connect to a paired device.
    Connect { address: String },
    /// Disconnect a device but keep the bond.
    Disconnect { address: String },
    /// Remove the bond with a device.
    Forget { address: String },
    /// Set or clear the trusted flag on a device.
    Trust { address: String, state: OnOff },
    /// Pair with a device interactively.
    Pair {
        address: String,
        /// Override the agent response timeout (seconds) for this
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
    /// List GNSS devices.
    List,
    /// Show the current fix for a GNSS device.
    Show { device: Option<String> },
    /// Show the satellite counts for a GNSS device.
    Satellites { device: Option<String> },
}

#[derive(Debug, Subcommand)]
pub enum ProfileSub {
    /// List stored profiles.
    List {
        #[arg(long, value_enum)]
        kind: Option<ProfileKind>,
    },
    /// Show a stored profile.
    Show { reference: String },
    /// Dump a profile's TOML to stdout.
    Export { reference: String },
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
    Remove { reference: String },
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
    /// Print the daemon's current PowerState.
    Get,
    /// Change the daemon's PowerState.
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
    /// Show the Profile Store master-key source and status.
    MasterKeyInfo,
    /// Rotate the Profile Store master key (fire-and-forget; prints
    /// the job id — watch `master-key-rotated` for the outcome).
    RotateMasterKey,
    /// Acquire a backup lease (prints the lease token).
    FreezeBackup,
    /// Release a previously-acquired backup lease.
    ReleaseBackup { lease: String },
    /// Request a support bundle. Planned feature — streaming the
    /// bundle fd lands in a later phase.
    Diagnostics {
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },
    /// Re-read nexus.toml and apply reloadable fields.
    ReloadConfig,
}
