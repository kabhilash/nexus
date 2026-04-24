//! `nexusctl completions <shell>` handler. DD-008 §10.
//!
//! Emits a working completion script on stdout. The script is the
//! composition of two layers:
//!
//! 1. The static body — subcommands, flags, value enums — produced
//!    by `clap_complete` from the [`crate::cli::Cli`] struct.
//! 2. A dynamic overlay that post-hooks positionals described in
//!    [`crate::completion::dynamic`] to `nexusctl --terse
//!    --fields=<f> ...` callbacks so `<iface>`, `<address>`,
//!    `<profile>` resolve to live candidates.
//!
//! For bash the overlay is fully wired: it defines per-slot helper
//! functions, wraps the clap-generated `_nexusctl` in
//! `__nexusctl_wrap` that delegates then augments, and re-registers
//! `complete -F __nexusctl_wrap nexusctl`. The other three shells
//! ship the static body plus a documentary comment block with the
//! slot table; porting the overlay to their native completion DSLs
//! is tracked as follow-up polish.
//!
//! Packagers install by redirecting output (per DD-008 §10):
//!
//! ```text
//! $ nexusctl completions bash > /etc/bash_completion.d/nexusctl
//! $ nexusctl completions zsh  > /usr/share/zsh/site-functions/_nexusctl
//! $ nexusctl completions fish > /usr/share/fish/vendor_completions.d/nexusctl.fish
//! ```

use std::io::Write;

use crate::completion::{Shell, dynamic, r#static};
use crate::errors::NexusctlError;

/// Emit the completion script for `shell` to `out`. Returns `Ok(())`
/// on success; an `io::Error` from `out` surfaces as
/// [`NexusctlError::IoError`].
pub fn run(shell: Shell, out: &mut dyn Write) -> Result<(), NexusctlError> {
    emit(shell, out).map_err(|e| NexusctlError::IoError {
        detail: e.to_string(),
    })
}

fn emit(shell: Shell, out: &mut dyn Write) -> std::io::Result<()> {
    r#static::write(shell, out)?;
    match shell {
        Shell::Bash => out.write_all(bash_overlay().as_bytes())?,
        Shell::Zsh => out.write_all(zsh_overlay_stub().as_bytes())?,
        Shell::Fish => out.write_all(fish_overlay_stub().as_bytes())?,
        Shell::PowerShell => out.write_all(pwsh_overlay_stub().as_bytes())?,
    }
    Ok(())
}

// ---- bash overlay ----------------------------------------------------------

fn bash_overlay() -> String {
    let mut s = String::new();
    s.push_str("\n# ---- nexusctl dynamic completion overlay (DD-008 §10) ----\n");
    s.push_str(BASH_PRELUDE);
    s.push('\n');
    // One helper function per slot.
    for slot in dynamic::all_slots() {
        s.push_str(&format!(
            "{fn_name}() {{\n    __nexusctl_dyn_add \"{argv}\"\n}}\n",
            fn_name = slot.bash_fn(),
            argv = slot.nexusctl_argv(),
        ));
    }
    s.push('\n');
    s.push_str(BASH_DISPATCHER_OPEN);
    for (chain, idx, slot) in dynamic::chain_slots() {
        // `idx` is the 0-based positional index after the subcommand
        // chain — chain has `n` words after `nexusctl`, so the
        // positional lands at COMP_CWORD == 1 + n + idx.
        let word_count = 1 + chain.split_whitespace().count() + idx;
        s.push_str(&format!(
            "        \"{chain}\")\n            if [[ $COMP_CWORD -eq {word_count} ]]; then\n                {fn_name}\n            fi\n            ;;\n",
            chain = chain,
            word_count = word_count,
            fn_name = slot.bash_fn(),
        ));
    }
    s.push_str(BASH_DISPATCHER_CLOSE);
    s
}

// The prelude defines `__nexusctl_dyn_add` (the callback
// `timeout`-guarded bridge) and the wrapper that defers to the
// clap-generated `_nexusctl` then augments.
const BASH_PRELUDE: &str = r#"
__nexusctl_dyn_add() {
    # Run the nexusctl callback under a hard 1s timeout so a hung
    # daemon can't freeze the shell. Silence stderr — completion is
    # a best-effort surface.
    local argv="$1"
    local out
    out="$(timeout 1 $argv 2>/dev/null)" || return 0
    local cur="${COMP_WORDS[COMP_CWORD]}"
    local candidates
    candidates="$(printf '%s\n' "$out")"
    local IFS=$'\n'
    # shellcheck disable=SC2207
    COMPREPLY+=( $(compgen -W "$candidates" -- "$cur") )
}

__nexusctl_subcommand_chain() {
    # Join words[1..COMP_CWORD-1] with single spaces, skipping global
    # flags and their arguments. Keeps the chain short so the case
    # labels stay readable (e.g. "wifi show").
    local chain=""
    local skip=0
    local i
    for ((i=1; i<COMP_CWORD; i++)); do
        local w="${COMP_WORDS[i]}"
        if (( skip )); then
            skip=0
            continue
        fi
        case "$w" in
            --format|--timeout|--bus|--color|--config|--fields|--separator|-f)
                skip=1
                ;;
            --json|--terse|--pretty|--no-color|--verbose|-v|--quiet|-q| \
            --no-interactive|--no-polkit-agent|--help|-h|--version)
                ;;
            -*)
                ;;
            *)
                if [[ -z "$chain" ]]; then
                    chain="$w"
                else
                    chain="$chain $w"
                fi
                ;;
        esac
    done
    printf '%s' "$chain"
}
"#;

const BASH_DISPATCHER_OPEN: &str = r#"__nexusctl_wrap() {
    # Delegate to the clap-generated static completion first so
    # flags and siblings still appear.
    _nexusctl "$@"

    local chain
    chain="$(__nexusctl_subcommand_chain)"
    case "$chain" in
"#;

const BASH_DISPATCHER_CLOSE: &str = r#"        *) ;;
    esac
}

# Override the clap-installed completion with our wrapper. Bash's
# `complete` uses last-wins semantics for the -F binding.
complete -F __nexusctl_wrap nexusctl
"#;

// ---- non-bash overlay stubs ----------------------------------------------

fn zsh_overlay_stub() -> String {
    header_comment(
        "zsh",
        "Dynamic completion for zsh is currently static-only. \
         To wire runtime candidates, add `_nexusctl_<slot>` helpers \
         that call the argv listed below inside a `timeout 1` guard.",
    )
}

fn fish_overlay_stub() -> String {
    header_comment(
        "fish",
        "Dynamic completion for fish is currently static-only. \
         Add `complete -c nexusctl -n '<condition>' -a '(timeout 1 <argv>)'` \
         lines for the slots below to enable runtime candidates.",
    )
}

fn pwsh_overlay_stub() -> String {
    header_comment(
        "PowerShell",
        "Dynamic completion for PowerShell is currently static-only. \
         Extend the Register-ArgumentCompleter block above with \
         per-positional callbacks invoking the argv listed below.",
    )
}

fn header_comment(shell: &str, summary: &str) -> String {
    let mut s = String::new();
    s.push_str("\n# ---- nexusctl dynamic completion hooks (DD-008 §10) ----\n");
    s.push_str(&format!("# {shell}: {summary}\n"));
    s.push_str("#\n# Slot                         --fields   argv\n");
    for slot in dynamic::all_slots() {
        s.push_str(&format!(
            "# {:<28} {:<10} {}\n",
            format!("{:?}", slot),
            slot.field(),
            slot.argv().join(" "),
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_overlay_defines_every_slot_helper() {
        let s = bash_overlay();
        for slot in dynamic::all_slots() {
            assert!(
                s.contains(&format!("{}()", slot.bash_fn())),
                "bash overlay missing helper for {slot:?}: {s}"
            );
        }
    }

    #[test]
    fn bash_overlay_registers_wrapper() {
        let s = bash_overlay();
        assert!(s.contains("complete -F __nexusctl_wrap nexusctl"));
        assert!(s.contains("__nexusctl_wrap()"));
    }

    #[test]
    fn bash_overlay_dispatches_known_chains() {
        let s = bash_overlay();
        assert!(s.contains("\"wifi show\")"));
        assert!(s.contains("\"bt pair\")"));
        assert!(s.contains("\"profile show\")"));
    }

    #[test]
    fn full_bash_output_includes_static_and_overlay() {
        let mut buf = Vec::new();
        emit(Shell::Bash, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Clap piece.
        assert!(s.contains("_nexusctl"));
        // Our overlay piece.
        assert!(s.contains("__nexusctl_wrap"));
        assert!(s.contains("__nexusctl_dyn_add"));
    }

    #[test]
    fn non_bash_shells_emit_documentary_stub_only() {
        for shell in [Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let mut buf = Vec::new();
            emit(shell, &mut buf).unwrap();
            let s = String::from_utf8(buf).unwrap();
            assert!(
                s.contains("Slot"),
                "{shell:?} overlay stub should document slot table",
            );
            // And crucially must NOT install the bash wrapper.
            assert!(
                !s.contains("__nexusctl_wrap"),
                "{shell:?} overlay leaked bash-only symbol"
            );
        }
    }
}
