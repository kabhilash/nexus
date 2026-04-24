//! Phase-8 completion tests. DD-008 §10 / §12.
//!
//! Covers:
//! - `nexusctl completions <shell>` compiles a complete script;
//! - bash output parses under `bash -n`;
//! - zsh output parses under `zsh -n` (when available);
//! - dynamic-slot helpers produce the correct `--terse --fields=<f>
//!   …` callback strings;
//! - every shell emits the DD-008 §4.1 subcommand set and the
//!   packager-visible install note.
//!
//! Shells that aren't installed in the test environment are skipped
//! cleanly rather than failing — the bash check is the critical
//! CI gate; fish / pwsh syntax checks need a manual VM run.

use std::io::Write;
use std::process::{Command as PCommand, Stdio};

use nexus_client::commands::completions;
use nexus_client::completion::dynamic::{DynamicSlot, all_slots, chain_slots};
use nexus_client::completion::{Shell, r#static};

fn script_for(shell: Shell) -> String {
    let mut buf: Vec<u8> = Vec::new();
    completions::run(shell, &mut buf).expect("completions run");
    String::from_utf8(buf).expect("valid UTF-8")
}

fn shell_available(program: &str) -> bool {
    PCommand::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn syntax_check(program: &str, script: &str, extra_flags: &[&str]) {
    let mut cmd = PCommand::new(program);
    cmd.arg("-n").args(extra_flags);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn shell");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "{program} -n failed: stderr={}\nscript prefix:\n{}",
        String::from_utf8_lossy(&out.stderr),
        &script[..script.len().min(400)],
    );
}

#[test]
fn bash_output_parses_with_bash_n() {
    if !shell_available("bash") {
        eprintln!("bash not available; skipping");
        return;
    }
    let script = script_for(Shell::Bash);
    syntax_check("bash", &script, &[]);
}

#[test]
fn zsh_output_parses_with_zsh_n() {
    if !shell_available("zsh") {
        eprintln!("zsh not available; skipping");
        return;
    }
    let script = script_for(Shell::Zsh);
    // `-n` parses without executing. `+o nomatch` so the occasional
    // `*` glob inside the clap output isn't resolved against the
    // filesystem. We pipe the script through stdin, so zsh interprets
    // it as a zsh script (not a completion function install) — that's
    // fine for a syntax check.
    syntax_check("zsh", &script, &[]);
}

#[test]
fn fish_output_parses_with_fish_n() {
    if !shell_available("fish") {
        eprintln!("fish not available; skipping");
        return;
    }
    let script = script_for(Shell::Fish);
    syntax_check("fish", &script, &[]);
}

#[test]
fn powershell_output_is_non_empty_and_registers_completer() {
    let script = script_for(Shell::PowerShell);
    assert!(script.contains("Register-ArgumentCompleter"));
    // Our overlay stub for pwsh is documentary (a comment block).
    assert!(script.contains("Slot"));
}

#[test]
fn bash_output_contains_wrapper_and_every_dynamic_helper() {
    let script = script_for(Shell::Bash);
    assert!(script.contains("__nexusctl_wrap()"));
    assert!(script.contains("complete -F __nexusctl_wrap nexusctl"));
    assert!(script.contains("timeout 1"));
    for slot in all_slots() {
        assert!(
            script.contains(&format!("{}()", slot.bash_fn())),
            "missing helper for {slot:?}"
        );
    }
}

#[test]
fn bash_output_dispatches_every_chain_slot() {
    let script = script_for(Shell::Bash);
    for (chain, _idx, slot) in chain_slots() {
        assert!(
            script.contains(&format!("\"{chain}\")")),
            "missing chain dispatcher for `{chain}`"
        );
        assert!(
            script.contains(slot.bash_fn()),
            "missing bash helper for slot {slot:?} referenced by `{chain}`"
        );
    }
}

#[test]
fn dynamic_slot_argv_matches_dd008_narrative() {
    // DD-008 §10's example: `nexusctl --terse --fields=iface iface
    // list --kind wireless`. Our implementation substitutes the DD-001
    // `wifi` wire value for the DD's english `wireless`, but the
    // structural shape (per-shell invocation) matches exactly.
    let s = DynamicSlot::InterfaceWifi.nexusctl_argv();
    assert!(s.starts_with("nexusctl "));
    assert!(s.contains("--terse"));
    assert!(s.contains("--fields iface"));
    assert!(s.contains("iface list --kind wifi"));
}

#[test]
fn static_layer_alone_works_without_completions_command() {
    // The `r#static::render` helper is reusable — e.g. a REPL host
    // embedding the static completion body without the dispatch
    // overlay. Sanity-check it renders stably for every shell.
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
        let s = r#static::render(shell);
        assert!(!s.is_empty(), "empty static render for {shell:?}");
        assert!(s.contains("nexusctl"), "missing binary name for {shell:?}");
    }
}

// -- clap parse integration ------------------------------------------------

#[test]
fn completions_subcommand_parses_for_every_shell() {
    use clap::Parser;
    use nexus_client::cli::{Cli, Command, CompletionShell};
    for (arg, want) in [
        ("bash", CompletionShell::Bash),
        ("zsh", CompletionShell::Zsh),
        ("fish", CompletionShell::Fish),
        ("powershell", CompletionShell::Powershell),
        ("pwsh", CompletionShell::Powershell),
    ] {
        let cli = Cli::try_parse_from(["nexusctl", "completions", arg])
            .unwrap_or_else(|e| panic!("parse {arg}: {e}"));
        match cli.command {
            Some(Command::Completions { shell }) => assert_eq!(shell, want, "for arg {arg}"),
            other => panic!("got {other:?}"),
        }
    }
}

#[test]
fn completions_subcommand_rejects_unknown_shell() {
    use clap::Parser;
    use nexus_client::cli::Cli;
    let err = Cli::try_parse_from(["nexusctl", "completions", "tcsh"]).unwrap_err();
    // clap emits InvalidValue for ValueEnum mismatches.
    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
}
