//! `nexusctl` library surface.
//!
//! The CLI's behaviour is split between the binary entry point
//! ([`main`](crate::main)) and a small library that everything else
//! lives in. Tests link against the library directly so they don't
//! have to spawn the binary for every assertion.
//!
//! Architecture (DD-008 §1.1):
//! - [`cli`]      — clap derive structs that own the public CLI shape.
//! - [`dispatch`] — routes parsed args to a command handler.
//! - [`proxy`]    — D-Bus proxy layer + the [`proxy::ManagerOps`]
//!                  trait that handlers depend on (so tests inject
//!                  a mock without spinning up zbus).
//! - [`commands`] — per-subcommand handlers. Each takes a
//!                  `&dyn ManagerOps` plus an [`output::OutputFormat`]
//!                  and writes to a generic `Write` sink.
//! - [`output`]   — Human (comfy-table) and JSON renderers.
//! - [`errors`]   — top-level `NexusctlError` per DD-008 §9.

pub mod cli;
pub mod commands;
pub mod dispatch;
pub mod errors;
pub mod output;
pub mod proxy;

pub use errors::{NexusctlError, exit_code_for};
pub use output::OutputFormat;
