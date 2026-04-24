//! clap_complete-backed static completion. DD-008 §10.
//!
//! The static body covers subcommands, long/short flags, value enums,
//! and anything else `clap` already knows about. Dynamic positions
//! (interface / address / profile) get an overlay from
//! [`super::dynamic`] — static completion still offers `--help` and
//! sibling flag names when the positional hasn't been typed yet, so
//! the shell is never "empty."

use std::io::Write;

use clap::CommandFactory;

use super::Shell;
use crate::cli::Cli;

/// Write the static completion script for `shell` to `out`.
///
/// Returns the number of bytes written. The caller is responsible for
/// appending any dynamic-completion overlay — see
/// [`crate::commands::completions::emit`] for the composed flow.
pub fn write(shell: Shell, out: &mut dyn Write) -> std::io::Result<()> {
    let mut cmd = Cli::command();
    // `bin_name` must be the executable name; clap_complete keys the
    // function names off it (`_nexusctl`, `__nexusctl_wrap`, …) and
    // the dynamic overlay relies on this being exactly `nexusctl`.
    let bin = "nexusctl";
    clap_complete::generate(shell.as_clap(), &mut cmd, bin, out);
    Ok(())
}

/// Convenience wrapper: render into a `String`.
pub fn render(shell: Shell) -> String {
    let mut buf = Vec::new();
    write(shell, &mut buf).expect("writing to Vec is infallible");
    String::from_utf8(buf).expect("clap_complete emits valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_output_contains_root_command() {
        let s = render(Shell::Bash);
        assert!(s.contains("_nexusctl"), "missing _nexusctl: {s}");
        assert!(s.contains("complete"), "missing complete builtin: {s}");
    }

    #[test]
    fn zsh_output_has_compdef_directive() {
        let s = render(Shell::Zsh);
        assert!(s.contains("#compdef nexusctl"), "missing compdef: {s}");
    }

    #[test]
    fn fish_output_has_complete_directive() {
        let s = render(Shell::Fish);
        assert!(
            s.contains("complete -c nexusctl"),
            "missing complete -c: {s}"
        );
    }

    #[test]
    fn powershell_output_mentions_register_argumentcompleter() {
        let s = render(Shell::PowerShell);
        assert!(
            s.contains("Register-ArgumentCompleter"),
            "missing Register-ArgumentCompleter: {s}"
        );
    }

    #[test]
    fn every_shell_lists_every_subcommand() {
        // Sanity check: whatever clap_complete emits, every top-level
        // verb we advertise in DD-008 §4.1 must appear as a literal
        // somewhere. Catches regressions where a new Command variant
        // is added to cli.rs but not actually compiled into the
        // completion (happens when attributes like `#[command(skip)]`
        // sneak in).
        let expected = [
            "status",
            "iface",
            "eth",
            "wifi",
            "bt",
            "gnss",
            "profile",
            "power",
            "admin",
            "watch",
            "completions",
        ];
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let s = render(shell);
            for sub in expected {
                assert!(
                    s.contains(sub),
                    "{shell:?} output missing subcommand `{sub}`",
                );
            }
        }
    }
}
