//! `nexusctl` binary entry point. Tiny — every interesting bit is
//! in the library (`nexus_client::*`).
//!
//! Lifecycle:
//!
//! 1. clap parses argv. Bad args → `clap::Error::exit` (exit 2).
//! 2. Tracing subscriber initialises (DEBUG when `--verbose`, else
//!    WARN). All logs go to stderr; stdout stays reserved for
//!    command output.
//! 3. Open a D-Bus connection (system bus, or `--bus` address for
//!    test harnesses).
//! 4. Dispatch to the command handler.
//! 5. Exit with the DD-008 §4.3 code.

use std::io::Write as _;
use std::process::ExitCode;

use clap::Parser;
use tracing::Level;

use nexus_client::cli::Cli;
use nexus_client::dispatch::dispatch;
use nexus_client::errors::NexusctlError;
use nexus_client::proxy::ZbusManagerOps;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    init_tracing(cli.global.verbose);

    let ops = match ZbusManagerOps::connect(cli.global.bus.as_deref()).await {
        Ok(o) => o,
        Err(e) => return finish(Err(e)),
    };

    let mut stdout = std::io::stdout().lock();
    let result = dispatch(&cli, &ops, &mut stdout).await;
    // Flush so the buffered table doesn't get clipped on early
    // exit. Ignore EPIPE — our stdout consumer hung up.
    let _ = stdout.flush();
    finish(result)
}

fn init_tracing(verbose: bool) {
    let level = if verbose { Level::DEBUG } else { Level::WARN };
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .try_init();
}

fn finish(result: Result<(), NexusctlError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Empty messages happen on EPIPE — stay silent.
            let msg = err.to_string();
            if !msg.is_empty() {
                let _ = writeln!(std::io::stderr(), "nexusctl: {msg}");
            }
            ExitCode::from(err.exit_code() as u8)
        }
    }
}
