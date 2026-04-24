//! Per-subcommand handlers. Each takes a `&dyn ManagerOps`, an
//! [`crate::output::OutputFormat`], a [`crate::output::RenderContext`],
//! and a `&mut dyn Write` so tests can capture output without
//! spawning the binary.

pub mod admin;
pub mod bt;
pub mod gnss;
pub mod iface;
pub mod power;
pub mod profile;
pub mod status;
