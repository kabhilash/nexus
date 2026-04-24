//! `nexusctl` library surface.
//!
//! The CLI's behaviour is split between the binary entry point
//! (`main`) and this library. Tests link against the library so
//! they don't have to spawn the binary for every assertion.
//!
//! Architecture (DD-008 §1.1):
//! - [`cli`]         — clap derive structs (public CLI shape).
//! - [`dispatch`]    — routes parsed args to a command handler.
//! - [`proxy`]       — D-Bus proxies + the [`proxy::ManagerOps`]
//!                     trait handlers depend on (so tests inject a
//!                     stub).
//! - [`commands`]    — per-subcommand handlers. Each builds a
//!                     view and hands off to [`output::render`].
//! - [`output`]      — Human / Terse / JSON / Pretty renderers.
//! - [`errors`] +
//!   [`errors_map`]  — the full DD-008 §9 variant set and a
//!                     zbus-error translator.
//! - [`state_prefix`] — per-kind state-badge classifier for the
//!                     human-mode `iface list` column.

pub mod cli;
pub mod commands;
pub mod dispatch;
pub mod errors;
pub mod errors_map;
pub mod output;
pub mod path_resolve;
pub mod proxy;
pub mod state_prefix;

pub use errors::{NexusctlError, exit_code_for};
pub use errors_map::{from_zbus_error, translate_method_error};
pub use output::{ColorChoice, OutputFormat, Render, RenderContext};
