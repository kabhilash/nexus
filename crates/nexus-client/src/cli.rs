//! Public CLI shape, defined with clap's derive API. DD-008 §4.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::output::{ColorChoice, OutputFormat, RenderContext};

/// `nexusctl` — the Nexus command-line client.
///
/// Talks to `nexusd` over D-Bus (`fi.nexus1`). One invocation, one
/// D-Bus connection, one command.
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

/// Global options per DD-008 §4.2. Every field here is `global =
/// true` in clap so it can appear on either side of the
/// subcommand.
#[derive(Debug, Args, Default, Clone)]
pub struct GlobalOpts {
    /// Output format.
    #[arg(global = true, long, short = 'f', value_enum)]
    pub format: Option<OutputFormat>,

    /// Shorthand for `--format json`. Mutually exclusive with the
    /// other format flags.
    #[arg(
        global = true,
        long,
        conflicts_with_all = ["format", "terse", "pretty"]
    )]
    pub json: bool,

    /// Shorthand for `--format terse`.
    #[arg(
        global = true,
        long,
        short = 't',
        conflicts_with_all = ["format", "json", "pretty"]
    )]
    pub terse: bool,

    /// Shorthand for `--format pretty`.
    #[arg(
        global = true,
        long,
        conflicts_with_all = ["format", "json", "terse"]
    )]
    pub pretty: bool,

    /// Comma-separated column selection for terse mode. Ignored
    /// by human / JSON / pretty renderers.
    #[arg(global = true, long, value_delimiter = ',')]
    pub fields: Option<Vec<String>>,

    /// Field separator in terse mode.
    #[arg(global = true, long, default_value = ":")]
    pub separator: String,

    /// Colour handling. `auto` (default) checks stdout for a TTY.
    #[arg(global = true, long, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Shorthand for `--color never`. Takes precedence over `--color`.
    #[arg(global = true, long)]
    pub no_color: bool,

    /// Override the per-command D-Bus call timeout, in seconds.
    /// Applied at the call site — Phase 2's two commands don't yet
    /// consult this, but later mutating calls do.
    #[arg(global = true, long, value_name = "SECONDS")]
    pub timeout: Option<u64>,

    /// D-Bus bus address. Default = system bus.
    #[arg(global = true, long, value_name = "ADDRESS")]
    pub bus: Option<String>,

    /// Bump stderr log level to DEBUG.
    #[arg(global = true, long, short = 'v')]
    pub verbose: bool,

    /// Suppress non-essential stderr output.
    #[arg(global = true, long, short = 'q')]
    pub quiet: bool,

    /// Refuse to prompt; fail with exit 5 if interaction would
    /// be required.
    #[arg(global = true, long)]
    pub no_interactive: bool,

    /// Alternative config file path. Honored by future phases;
    /// no-op today.
    #[arg(global = true, long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

impl GlobalOpts {
    /// Resolve the effective output format. Precedence: the first
    /// shorthand flag wins (`--json` / `--terse` / `--pretty`);
    /// otherwise `--format`; otherwise `NEXUSCTL_FORMAT`;
    /// otherwise Human.
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
                // Unknown env value: fall through to the default.
                _ => OutputFormat::Human,
            };
        }
        OutputFormat::Human
    }

    /// Resolve the colour choice. `--no-color` overrides `--color`.
    pub fn color_choice(&self) -> ColorChoice {
        if self.no_color {
            ColorChoice::Never
        } else {
            self.color
        }
    }

    /// Build a [`RenderContext`] for the selected format. Merges
    /// terse options, colour choice, etc.
    pub fn render_context(&self) -> RenderContext {
        RenderContext {
            fields: self.fields.clone(),
            separator: self.separator.clone(),
            color: self.color_choice(),
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
}

#[derive(Debug, Subcommand)]
pub enum IfaceSub {
    /// List every interface nexusd has discovered.
    List,
}
