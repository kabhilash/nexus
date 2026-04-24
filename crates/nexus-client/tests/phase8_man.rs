//! Man page generation. DD-008 §10 / §12 Phase 8.
//!
//! `clap_mangen` renders a groff(7) page straight from the
//! [`nexus_client::cli::Cli`] struct. The default test renders
//! in-memory and checks that every top-level subcommand + key
//! sections are present. The `#[ignore]`-gated `regenerate` test
//! writes the output to `packaging/man/nexusctl.1` — run it when the
//! CLI surface changes to refresh the checked-in page:
//!
//! ```text
//! cargo test -p nexus-client --test phase8_man -- --ignored regenerate
//! ```
//!
//! That keeps the packaging tree in the repo without putting
//! `clap_mangen` in the production dependency set.

use std::path::PathBuf;

use clap::CommandFactory;
use clap_mangen::Man;

use nexus_client::cli::Cli;

fn render_root() -> String {
    let cmd = Cli::command();
    let man = Man::new(cmd);
    let mut buf: Vec<u8> = Vec::new();
    man.render(&mut buf).expect("render");
    String::from_utf8(buf).expect("valid UTF-8")
}

#[test]
fn root_man_page_names_every_top_level_subcommand() {
    let page = render_root();
    // `.SH` is the groff section header macro clap_mangen emits.
    assert!(page.contains(".SH NAME"), "missing NAME section");
    assert!(page.contains(".SH SYNOPSIS"), "missing SYNOPSIS section");
    // DD-008 §4.1 top-level verbs.
    for verb in [
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
    ] {
        assert!(page.contains(verb), "man page missing subcommand `{verb}`");
    }
}

#[test]
fn root_man_page_lists_global_flags() {
    let page = render_root();
    // clap_mangen emits flags in groff-escaped form: `-` becomes
    // `\-` so the man page renderer doesn't treat it as a soft
    // hyphen. Every check below uses the escaped spelling.
    for flag in [
        r"\-\-format",
        r"\-\-json",
        r"\-\-terse",
        r"\-\-pretty",
        r"\-\-no\-color",
        r"\-\-color",
        r"\-\-timeout",
        r"\-\-bus",
        r"\-\-verbose",
        r"\-\-quiet",
        r"\-\-no\-interactive",
        r"\-\-config",
    ] {
        assert!(page.contains(flag), "man page missing flag `{flag}`");
    }
}

/// Regenerate `packaging/man/nexusctl.1`. Run with `--ignored` when
/// the CLI surface changes.
#[test]
#[ignore = "writes to the repo tree — run manually via --ignored"]
fn regenerate() {
    let page = render_root();
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "packaging",
        "man",
        "nexusctl.1",
    ]
    .iter()
    .collect();
    std::fs::create_dir_all(path.parent().unwrap()).expect("create packaging/man");
    std::fs::write(&path, page.as_bytes())
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    eprintln!("wrote {}", path.display());
}
