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
        /// Omit to use the single Wi-Fi interface, if exactly one
        /// is registered; ambiguous otherwise.
        iface: Option<String>,
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
        /// ULID or `Label` value.
        reference: String,
    },
    Export {
        /// ULID or `Label`.
        reference: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum PowerSub {
    /// Read the current `PowerState`.
    Get,
}

#[derive(Debug, Subcommand)]
pub enum AdminSub {
    /// Read `MasterKeySource` + related diagnostics.
    MasterKeyInfo,
}
