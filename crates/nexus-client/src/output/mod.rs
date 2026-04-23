//! Output formats. DD-008 §5.
//!
//! Phase 1 wires Human (default; comfy-table) and JSON. `Terse` and
//! `Pretty` are spec'd in DD-008 §5.2 / §5.4 and land in Phase 2.

use clap::ValueEnum;

pub mod human;
pub mod json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum OutputFormat {
    Human,
    Json,
}

impl Default for OutputFormat {
    fn default() -> Self {
        OutputFormat::Human
    }
}
