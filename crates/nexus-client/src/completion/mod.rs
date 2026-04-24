//! Shell completion generation. DD-008 §10.
//!
//! Two halves:
//!
//! - [`r#static`] — static subcommand/flag completion emitted by
//!   `clap_complete` straight from [`crate::cli::Cli`].
//! - [`dynamic`] — per-slot helpers that describe the `nexusctl
//!   --terse --fields=<f> ...` callbacks shell completion scripts
//!   invoke to turn `<iface>` / `<address>` / `<profile>` into live
//!   candidate lists.
//!
//! The `completions` subcommand in [`crate::commands::completions`]
//! glues the two together: static script first, then a per-shell
//! dynamic overlay (bash is fully wired; zsh / fish / pwsh ship the
//! static body plus a documented hook stub until the follow-up
//! that ports the overlay to their respective completion DSLs).

pub mod dynamic;
pub mod r#static;

use clap_complete::Shell as ClapShell;

/// Shells `nexusctl completions` can emit for. Mirrors
/// [`clap_complete::Shell`] but restricted to the four DD-008 §10
/// lists (no Elvish).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

impl Shell {
    /// Resolve from the `clap` ValueEnum representation.
    pub fn as_clap(self) -> ClapShell {
        match self {
            Shell::Bash => ClapShell::Bash,
            Shell::Zsh => ClapShell::Zsh,
            Shell::Fish => ClapShell::Fish,
            Shell::PowerShell => ClapShell::PowerShell,
        }
    }

    /// Lower-case name as accepted on the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::PowerShell => "powershell",
        }
    }
}
