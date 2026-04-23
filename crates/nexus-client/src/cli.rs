//! Public CLI shape, defined with clap's derive API. DD-008 §4.
//!
//! Phase 1 wires the global options (`--format`, `--json`, `--bus`,
//! `--verbose`) and two subcommands (`status`, `iface list`). The
//! tree skeleton is laid out so later phases drop in additional
//! domains without a structural rewrite.

use clap::{Args, Parser, Subcommand};

use crate::output::OutputFormat;

/// `nexusctl` — the Nexus command-line client.
///
/// Talks to `nexusd` over D-Bus (`fi.nexus1`). One invocation, one
/// D-Bus connection, one command. See `nexusctl <subcommand> --help`
/// for per-command details.
#[derive(Debug, Parser)]
#[command(
    name = "nexusctl",
    version,
    about = "Command-line client for nexusd",
    long_about = None,
    // DD-008 §4.1: accept any unambiguous prefix of a subcommand
    // (e.g. `nexusctl stat` → `nexusctl status`).
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

#[derive(Debug, Args, Default, Clone)]
pub struct GlobalOpts {
    /// Output format. `--json` is shorthand for `--format json`.
    #[arg(global = true, long, short = 'f', value_enum)]
    pub format: Option<OutputFormat>,

    /// Shorthand for `--format json`. Conflicts with `--format`.
    #[arg(global = true, long, conflicts_with = "format")]
    pub json: bool,

    /// D-Bus bus address. Default = system bus. Used by tests
    /// running against a private `dbus-daemon`.
    #[arg(global = true, long, value_name = "ADDRESS")]
    pub bus: Option<String>,

    /// Increase log verbosity to DEBUG on stderr (DD-008 §4.2).
    #[arg(global = true, long, short = 'v')]
    pub verbose: bool,
}

impl GlobalOpts {
    /// Resolve the effective output format: `--json` → Json,
    /// otherwise `--format`, otherwise the default (Human). Phase 2
    /// extends this to consult `NEXUSCTL_FORMAT` and the config file.
    pub fn output_format(&self) -> OutputFormat {
        if self.json {
            OutputFormat::Json
        } else {
            self.format.unwrap_or_default()
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show overall daemon status — version, power state, interface
    /// and profile counts.
    Status,
    /// Interface operations.
    Iface {
        #[command(subcommand)]
        sub: IfaceSub,
    },
}

#[derive(Debug, Subcommand)]
pub enum IfaceSub {
    /// List every interface nexusd has discovered.
    List,
}
