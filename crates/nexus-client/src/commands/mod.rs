//! Per-subcommand handlers. Each takes a `&dyn ManagerOps`, an
//! [`crate::output::OutputFormat`], and a `&mut dyn Write` so tests
//! can capture output without spawning a subprocess.

pub mod iface;
pub mod status;
