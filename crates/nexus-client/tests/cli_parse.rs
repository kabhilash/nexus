//! clap argument parsing for `nexusctl`. DD-008 §4.1 + §4.2.

use clap::Parser;
use nexus_client::cli::{Cli, Command, IfaceSub};
use nexus_client::output::OutputFormat;

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    let mut argv = vec!["nexusctl"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv)
}

#[test]
fn no_subcommand_is_allowed_and_status_is_default() {
    let cli = parse(&[]).expect("no-args invocation");
    assert!(cli.command.is_none());
}

#[test]
fn status_subcommand_parses() {
    let cli = parse(&["status"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Status)));
}

#[test]
fn iface_list_subcommand_parses() {
    let cli = parse(&["iface", "list"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Iface {
            sub: IfaceSub::List { kind: None }
        })
    ));
}

#[test]
fn iface_list_accepts_kind_filter() {
    let cli = parse(&["iface", "list", "--kind", "wifi"]).unwrap();
    match cli.command {
        Some(Command::Iface {
            sub: IfaceSub::List { kind },
        }) => {
            assert_eq!(kind.map(|k| k.as_wire()), Some("wifi"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn watch_without_subcommand_parses() {
    use nexus_client::cli::WatchSub;
    let cli = parse(&["watch"]).unwrap();
    match cli.command {
        Some(Command::Watch { sub, filter }) => {
            assert!(sub.is_none());
            assert!(filter.is_empty());
            // Quiet unused-import lint.
            let _: Option<WatchSub> = None;
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn watch_subsets_parse() {
    use nexus_client::cli::WatchSub;
    for (arg, want) in [
        ("events", WatchSub::Events),
        ("iface", WatchSub::Iface),
        ("wifi", WatchSub::Wifi),
        ("bt", WatchSub::Bt),
        ("gnss", WatchSub::Gnss),
    ] {
        let cli = parse(&["watch", arg]).unwrap();
        let actual = match cli.command {
            Some(Command::Watch { sub: Some(s), .. }) => s,
            other => panic!("got {other:?}"),
        };
        assert!(
            std::mem::discriminant(&actual) == std::mem::discriminant(&want),
            "arg {arg} produced wrong variant"
        );
    }
}

#[test]
fn watch_filter_flag_is_repeatable() {
    let cli = parse(&["watch", "--filter", "iface=eth0", "--filter", "kind=link-*"]).unwrap();
    match cli.command {
        Some(Command::Watch { filter, .. }) => {
            assert_eq!(filter, vec!["iface=eth0", "kind=link-*"]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn no_polkit_agent_flag_is_global() {
    let cli = parse(&["--no-polkit-agent", "bt", "pair", "AA:BB:CC:DD:EE:01"]).unwrap();
    assert!(cli.global.no_polkit_agent);
}

#[test]
fn command_is_mutating_classifier_matches_dd008() {
    use nexus_client::cli::{
        AdminSub, BtSub, Command, OnOff, PowerStateArg, PowerSub, ProfileSub, WifiSub,
    };
    use nexus_client::dispatch::command_is_mutating;

    // Read-only paths.
    assert!(!command_is_mutating(&None));
    assert!(!command_is_mutating(&Some(Command::Status)));

    // A sampling of mutating paths.
    assert!(command_is_mutating(&Some(Command::Wifi {
        sub: WifiSub::Disconnect { iface: None }
    })));
    assert!(command_is_mutating(&Some(Command::Bt {
        sub: BtSub::Power {
            hci: "hci0".into(),
            state: OnOff::On
        }
    })));
    assert!(command_is_mutating(&Some(Command::Profile {
        sub: ProfileSub::Remove {
            reference: "x".into()
        }
    })));
    assert!(command_is_mutating(&Some(Command::Power {
        sub: PowerSub::Set {
            state: PowerStateArg::Active
        }
    })));
    assert!(command_is_mutating(&Some(Command::Admin {
        sub: AdminSub::RotateMasterKey
    })));
    assert!(!command_is_mutating(&Some(Command::Admin {
        sub: AdminSub::MasterKeyInfo
    })));
}

#[test]
fn bt_pair_parses_with_default_timeout() {
    use nexus_client::cli::BtSub;
    let cli = parse(&["bt", "pair", "AA:BB:CC:DD:EE:01"]).unwrap();
    match cli.command {
        Some(Command::Bt {
            sub: BtSub::Pair { address, timeout },
        }) => {
            assert_eq!(address, "AA:BB:CC:DD:EE:01");
            assert_eq!(timeout, 90);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn bt_list_paired_connected_conflict() {
    assert_eq!(
        parse(&["bt", "list", "--paired", "--connected"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::ArgumentConflict
    );
}

#[test]
fn profile_show_requires_reference() {
    let err = parse(&["profile", "show"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
}

#[test]
fn unknown_subcommand_is_rejected() {
    let err = parse(&["bogus"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
}

#[test]
fn abbreviated_subcommand_resolves() {
    // DD-008 §4.1 promises any unambiguous prefix works.
    let cli = parse(&["stat"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Status)));
}

#[test]
fn json_flag_overrides_default_format() {
    let cli = parse(&["--json", "iface", "list"]).unwrap();
    assert!(cli.global.json);
    assert_eq!(cli.global.output_format(), OutputFormat::Json);
}

#[test]
fn explicit_format_human_is_default() {
    let cli = parse(&["--format", "human", "status"]).unwrap();
    assert_eq!(cli.global.output_format(), OutputFormat::Human);
}

#[test]
fn json_and_format_conflict() {
    // `--json` is shorthand for `--format json`; DD-008 §4.2 says
    // they're mutually exclusive (clap enforces this).
    let err = parse(&["--json", "--format", "human", "status"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
}

#[test]
fn bus_address_global_option_round_trips() {
    let cli = parse(&["--bus", "unix:path=/tmp/bus", "status"]).unwrap();
    assert_eq!(cli.global.bus.as_deref(), Some("unix:path=/tmp/bus"));
}

#[test]
fn verbose_short_flag_works() {
    let cli = parse(&["-v", "status"]).unwrap();
    assert!(cli.global.verbose);
}

#[test]
fn version_flag_exits_with_clap_display_error() {
    let err = parse(&["--version"]).unwrap_err();
    // clap models --version as a "display version" error so callers
    // know it's not a real failure.
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
}

#[test]
fn help_flag_exits_with_clap_display_error() {
    let err = parse(&["--help"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
}

// ---- Phase 2 global flags -------------------------------------------

#[test]
fn terse_flag_selects_terse_format() {
    let cli = parse(&["--terse", "iface", "list"]).unwrap();
    assert!(cli.global.terse);
    assert_eq!(cli.global.output_format(), OutputFormat::Terse);
}

#[test]
fn pretty_flag_selects_pretty_format() {
    let cli = parse(&["--pretty", "iface", "list"]).unwrap();
    assert!(cli.global.pretty);
    assert_eq!(cli.global.output_format(), OutputFormat::Pretty);
}

#[test]
fn shorthand_flags_conflict_with_each_other() {
    assert_eq!(
        parse(&["--terse", "--json", "iface", "list"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse(&["--pretty", "--json", "iface", "list"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse(&["--terse", "--pretty", "iface", "list"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::ArgumentConflict
    );
}

#[test]
fn fields_option_parses_comma_separated() {
    let cli = parse(&["--fields", "iface,state", "iface", "list"]).unwrap();
    assert_eq!(
        cli.global.fields,
        Some(vec!["iface".into(), "state".into()])
    );
}

#[test]
fn separator_option_round_trips() {
    let cli = parse(&["--separator", "\t", "iface", "list"]).unwrap();
    assert_eq!(cli.global.separator, "\t");
}

#[test]
fn default_separator_is_colon() {
    let cli = parse(&["iface", "list"]).unwrap();
    assert_eq!(cli.global.separator, ":");
}

#[test]
fn no_color_overrides_color() {
    let cli = parse(&["--color", "always", "--no-color", "iface", "list"]).unwrap();
    assert!(cli.global.no_color);
    // color_choice() promotes --no-color to Never.
    assert_eq!(
        cli.global.color_choice(),
        nexus_client::output::ColorChoice::Never
    );
}

#[test]
fn timeout_parses_as_seconds() {
    let cli = parse(&["--timeout", "45", "iface", "list"]).unwrap();
    assert_eq!(cli.global.timeout, Some(45));
}

#[test]
fn quiet_and_no_interactive_are_recognized() {
    let cli = parse(&["--quiet", "--no-interactive", "status"]).unwrap();
    assert!(cli.global.quiet);
    assert!(cli.global.no_interactive);
}

#[test]
fn config_path_is_optional() {
    let cli = parse(&["--config", "/tmp/nexusctl.toml", "status"]).unwrap();
    assert_eq!(
        cli.global.config.as_deref(),
        Some(std::path::Path::new("/tmp/nexusctl.toml"))
    );
}
