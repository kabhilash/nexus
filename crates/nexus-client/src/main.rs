//! `nexusctl` binary entry point.
//!
//! Lifecycle:
//! 1. clap parses argv. Bad args → `clap::Error::exit` (exit 2).
//! 2. Tracing subscriber initialises (DEBUG when `--verbose`, else
//!    WARN). All logs go to stderr.
//! 3. Open a D-Bus connection.
//! 4. Dispatch to the command handler.
//! 5. Exit with the DD-008 §4.3 code. Errors in JSON mode go to
//!    stderr as the DD-008 §9 envelope; otherwise as a plain text
//!    line.

use std::io::Write as _;
use std::process::ExitCode;

use clap::Parser;
use tracing::Level;

use nexus_client::cli::{Cli, Command};
use nexus_client::commands;
use nexus_client::dispatch::dispatch;
use nexus_client::errors::NexusctlError;
use nexus_client::output::OutputFormat;
use nexus_client::output::json;
use nexus_client::proxy::ZbusManagerOps;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    init_tracing(cli.global.verbose);

    let format = cli.global.output_format();

    // `completions` is self-contained — it reads the Cli struct,
    // generates a script, and writes to stdout. No D-Bus connection,
    // no polkit agent, no watchdog. Packagers invoke this in sterile
    // chroots where even opening the bus would fail; short-circuit
    // before any of that machinery spins up.
    if let Some(Command::Completions { shell }) = &cli.command {
        let mut stdout = std::io::stdout().lock();
        let r = commands::completions::run(shell.as_completion_shell(), &mut stdout);
        let _ = stdout.flush();
        return finish(r, format);
    }

    // Install the double-Ctrl-C watchdog early so it's active
    // across every command, not just mutating ones. Read-only
    // commands barely have time to notice the first SIGINT before
    // they'd exit anyway, so the cost is negligible.
    let _watchdog = nexus_client::interactive::cancellation::install_double_sigint_watchdog();

    // Spawn `pkttyagent` when the command class is likely to need
    // PolicyKit. We can't perfectly classify from CLI-parse alone
    // without a full reflection of every subcommand, so the rule
    // is: if the operator didn't opt out *and* the subcommand is
    // one we know mutates, auto-spawn. The guard's Drop releases
    // the process on every exit path.
    let wants_agent = !cli.global.no_polkit_agent
        && !matches!(
            std::env::var("NEXUSCTL_NO_POLKIT_AGENT").as_deref(),
            Ok("1")
        )
        && nexus_client::dispatch::command_is_mutating(&cli.command);
    let _polkit = nexus_client::interactive::polkit::PolkitAgent::maybe_spawn(wants_agent);

    let ops = match ZbusManagerOps::connect(cli.global.bus.as_deref()).await {
        Ok(o) => o,
        Err(e) => return finish(Err(e), format),
    };

    let mut stdout = std::io::stdout().lock();
    let mut stderr_lock = std::io::stderr().lock();
    let result = dispatch(&cli, &ops, &mut stdout, &mut stderr_lock).await;
    let _ = stdout.flush();
    drop(stderr_lock);
    finish(result, format)
}

fn init_tracing(verbose: bool) {
    let level = if verbose { Level::DEBUG } else { Level::WARN };
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .try_init();
}

fn finish(result: Result<(), NexusctlError>, format: OutputFormat) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `__CANCELLED__` is the sentinel the pairing flow
            // uses to ask for exit 130 (standard SIGINT code) with
            // no extra stderr noise. DD-008 §6.4 spells this code
            // out; the pairing handler writes the "pairing
            // cancelled" line via its own prompt impl.
            if let NexusctlError::Other { raw } = &err {
                if raw == "__CANCELLED__" {
                    return ExitCode::from(130u8);
                }
            }
            report_error(&err, format);
            ExitCode::from(err.exit_code() as u8)
        }
    }
}

fn report_error(err: &NexusctlError, format: OutputFormat) {
    // Empty messages happen on EPIPE — stay silent.
    let msg = err.to_string();
    if msg.is_empty() {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    match format {
        OutputFormat::Json => {
            // DD-008 §5.3: "Errors in JSON mode write a JSON error
            // object to stderr and exit non-zero." Compact (no
            // newlines within the object) so shell loops can read
            // one record per line.
            let _ = json::write_error_object(err, &mut stderr);
        }
        _ => {
            let _ = writeln!(stderr, "nexusctl: {msg}");
        }
    }
}
