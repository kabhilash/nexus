//! Per-subcommand handlers. Each takes a `&dyn ManagerOps`, an
//! [`crate::output::OutputFormat`], a [`crate::output::RenderContext`],
//! and a `&mut dyn Write` so tests can capture output without
//! spawning the binary.
//!
//! Mutating commands additionally take a `&mut dyn Write` for
//! stderr (used by the `--psk` leak warning) — see
//! [`crate::psk_warn`].

pub mod admin;
pub mod bt;
pub mod bt_mutating;
pub mod completions;
pub mod gnss;
pub mod iface;
pub mod power;
pub mod profile;
pub mod profile_mutating;
pub mod status;
pub mod watch;
pub mod wifi;
