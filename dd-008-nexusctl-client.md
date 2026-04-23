# DD-008: nexusctl — Command-Line Client

**Parent:** [Nexus Architecture](./nexus-architecture.md)
**Depends on:** [DD-006: D-Bus API](./dd-006-dbus-api.md)
**Referenced by:** none (leaf component)
**Status:** Draft
**Scope:** Design of `nexusctl`, the command-line client for Nexus. Wraps `fi.nexus1` D-Bus API in a scriptable and human-friendly CLI. Covers command structure, output formats, interactive flows (pairing, Wi-Fi auth, PolicyKit), signal subscription, error translation, and shell completion.

---

## Table of Contents

1. [Context](#1-context)
   - 1.1 [Repo Layout](#11-repo-layout)
2. [Responsibilities](#2-responsibilities)
3. [Failure Modes](#3-failure-modes)
4. [Command Structure](#4-command-structure)
   - 4.1 [Command Tree](#41-command-tree)
   - 4.2 [Global Options](#42-global-options)
   - 4.3 [Exit Codes](#43-exit-codes)
5. [Output Formats](#5-output-formats)
   - 5.1 [Default (Human)](#51-default-human)
   - 5.2 [Terse](#52-terse)
   - 5.3 [JSON](#53-json)
   - 5.4 [Pretty](#54-pretty)
6. [Interactive Flows](#6-interactive-flows)
   - 6.1 [Bluetooth Pairing](#61-bluetooth-pairing)
   - 6.2 [Wi-Fi Passphrase Entry](#62-wi-fi-passphrase-entry)
   - 6.3 [PolicyKit Authorization](#63-policykit-authorization)
   - 6.4 [Signal Handling and Cancellation](#64-signal-handling-and-cancellation)
   - 6.5 [Profile Edit Round-Trip](#65-profile-edit-round-trip)
7. [D-Bus Client Architecture](#7-d-bus-client-architecture)
   - 7.1 [Connection](#71-connection)
   - 7.2 [Proxy Layer](#72-proxy-layer)
   - 7.3 [Signal Subscription](#73-signal-subscription)
   - 7.4 [The `watch` Command](#74-the-watch-command)
8. [Configuration](#8-configuration)
9. [Error Translation](#9-error-translation)
10. [Shell Completion](#10-shell-completion)
11. [Testing Strategy](#11-testing-strategy)
12. [Implementation Phases](#12-implementation-phases)

---

## 1. Context

Nexus exposes its entire management surface via D-Bus (`fi.nexus1`, per DD-006). D-Bus is a great *programmatic* API — every language has a D-Bus client — but a poor *human* interface. Operators don't navigate `busctl tree fi.nexus1` to diagnose a Wi-Fi problem, and shell scripts don't want to build arrays of variant dicts by hand.

`nexusctl` fills this gap: a single command-line tool that wraps every fi.nexus1 operation in an ergonomic verb-noun CLI, modeled on `connmanctl` (ConnMan's client) with presentation borrowed from `nmcli` and pairing-UX from `bluetoothctl`. It serves three audiences:

1. **Operators** diagnosing a device over SSH (`nexusctl iface list`, `nexusctl bt pair AA:BB:…`).
2. **Integrators** writing systemd units, init scripts, or health checks (`nexusctl --terse wifi status`, `nexusctl profile add-wifi --ssid corp --psk "$PSK"`).
3. **Developers** exercising Nexus end-to-end during development and CI (`nexusctl --json iface list | jq '.[] | select(.kind == "wireless")'`).

It is not a management console or TUI dashboard. It does one thing per invocation and exits. Long-running operations (`watch`, interactive pairing) are explicit and bounded.

Competitors worth borrowing from, in order of relevance:

- **`connmanctl`** — the closest analog. ConnMan is the system Nexus replaces, and its CLI is the tool operators are familiar with on embedded Linux. Key patterns worth adopting: the **agent-registration model** for interactive credential prompts (nexusctl's pairing and Wi-Fi-PSK flows mirror this directly), the **compact state-prefix notation** in list output (`*AO` = autoconnected, online) that shows machine state without adding columns, and the **dual one-shot-plus-REPL** invocation style. What *not* to adopt: connmanctl's terse-by-default human output (hard to read when exploring), and its leaky service-name encoding (`wifi_aabbccddeeff_ssid_managed_psk`) — nexusctl uses addresses, SSIDs, and ULIDs as presented, not mangled internal identifiers.
- **`nmcli`** — NetworkManager's client. Best in class for human-readable table output and for a mature `--terse` mode. nexusctl borrows the column-oriented default format and the `--terse --fields=` selector pattern.
- **`bluetoothctl`** — interactive REPL. Useful for understanding what Bluetooth-pairing UI needs to look like, but REPL-first is the wrong default for scripting. nexusctl offers `nexusctl shell` as a secondary mode rather than a primary one.
- **`iw`** — thin netlink wrapper, terse by default. Too terse for nexusctl's audience.
- **`wpa_cli`** — interactive-first. Same limitation as `bluetoothctl`.

The mental model: **nexusctl is connmanctl's successor with nmcli's output sensibility and bluetoothctl's interactive-pairing flow.**

Within the workspace, nexusctl is a separate binary crate (`nexus-client`) so it can be packaged independently: a container or embedded firmware might ship only `nexusctl` to exercise a remote nexusd, while dev machines get both.

### 1.1 Repo Layout

```
crates/
  nexus-client/                 <- nexusctl binary
    Cargo.toml
    src/
      main.rs                   <- argument parsing + dispatch
      cli.rs                    <- clap derive structs for every subcommand
      dispatch.rs               <- maps parsed args to handler fns
      output/
        mod.rs                  <- OutputFormat enum + dispatch
        human.rs                <- tables, colors, box-drawing
        terse.rs                <- shell-script-friendly one-line records
        json.rs                 <- serde_json emission
        pretty.rs               <- verbose multi-line for single-record display
      commands/
        status.rs               <- nexusctl status
        iface.rs                <- nexusctl iface ...
        eth.rs                  <- nexusctl eth ...
        wifi.rs                 <- nexusctl wifi ...
        bt.rs                   <- nexusctl bt ...
        gnss.rs                 <- nexusctl gnss ...
        profile.rs              <- nexusctl profile ...
        power.rs                <- nexusctl power ...
        admin.rs                <- nexusctl admin ...
        watch.rs                <- nexusctl watch ...
        shell.rs                <- nexusctl shell (REPL)
        completions.rs          <- nexusctl completions <shell>
      interactive/
        mod.rs
        pairing.rs              <- Bluetooth pairing flow (§6.1)
        passphrase.rs           <- Wi-Fi PSK entry (§6.2)
        polkit.rs               <- PolicyKit agent spawning (§6.3)
      proxy/                    <- thin wrappers over zbus proxies
        mod.rs
        manager.rs
        interface.rs
        wifi.rs
        bt.rs
        gnss.rs
        profile.rs
      errors.rs                 <- fi.nexus.Error.* → human message map
      config.rs                 <- nexusctl-specific config (not nexus.toml)
      shell_completion.rs       <- generated by clap_complete
    tests/
      cli_parse.rs              <- clap argument parsing
      output_format.rs          <- snapshot tests on rendered output
      proxy_mock.rs             <- handler tests with mock zbus proxies
```

**Key dependencies:**

| Crate | Purpose |
|---|---|
| `clap` (v4, derive) | CLI parsing, help generation, shell completion |
| `zbus` (v5, tokio) | D-Bus client (same version as nexusd) |
| `tokio` | Async runtime for zbus |
| `serde` / `serde_json` | JSON output mode |
| `dialoguer` | Interactive prompts (passphrase, confirm) |
| `comfy-table` | Human-readable tables |
| `anstream` / `colored` | Terminal color handling |
| `clap_complete` | Shell completion generation |
| `tracing-subscriber` | Optional `--verbose` log output |

No dependency on nexus-core or the backend crates — nexusctl only talks to nexusd via D-Bus. This keeps nexusctl buildable and deployable without the full Nexus build.

---

## 2. Responsibilities

`nexusctl` is responsible for:

1. **Parsing CLI arguments** into structured commands with clap derive macros.
2. **Dispatching to a handler** that translates the command into one or more D-Bus calls against `fi.nexus1`.
3. **Rendering output** in the requested format (human table, terse one-liner, JSON, or pretty multi-line).
4. **Driving interactive flows** that require terminal UI: Bluetooth pairing prompts, Wi-Fi passphrase entry, PolicyKit authentication.
5. **Subscribing to D-Bus signals** and surfacing them via `nexusctl watch`.
6. **Translating D-Bus errors** (`fi.nexus.Error.*`, `org.freedesktop.DBus.Error.*`) into human-readable messages.
7. **Generating shell completions** for bash, zsh, fish, and PowerShell.
8. **Spawning a PolicyKit agent** when needed so operator authentication works over SSH.

Explicitly **not** responsible for:

- **Managing Nexus state directly.** Every operation goes through D-Bus; nexusctl has no writable state of its own (beyond a small config file for defaults).
- **TUI dashboards or long-running management consoles.** `nexusctl watch` is a tail, not a UI.
- **Supporting multiple nexusd instances over a network.** D-Bus is local-only; for remote use, operators SSH into the device first.
- **Being a D-Bus exploration tool.** For that, `busctl` exists.
- **Bundling PolicyKit policy files or installing systemd units.** That's nexusd's job (see DD-006 §10 and the packaging story in the nexus-daemon crate).

---

## 3. Failure Modes

**nexusd not running.** D-Bus returns `org.freedesktop.DBus.Error.ServiceUnknown` when activating `fi.nexus1`. nexusctl prints "nexusd is not running (try `systemctl start nexus`)" and exits with code 6.

**Insufficient privileges.** PolicyKit denies the operation. fi.nexus.Error.AuthFailed. nexusctl prints a message explaining what permission is required and how to elevate (e.g., "run as root" or "authenticate with an admin account"). Exit code 3.

**D-Bus call timeout.** Default zbus timeout is 25 s; long operations (pairing, scanning) may approach this. nexusctl uses per-call timeouts from §8 config, with sensible defaults per command class. Timeout prints "operation timed out after Ns" and exits with code 4.

**Signal subscription dropped.** If `nexusctl watch` loses its D-Bus connection mid-run (nexusd restart, or session bus death), it exits cleanly (exit code 0, message "nexusd disconnected") so shell scripts don't hang waiting on a dead stream. Operators who want to restart the subscription automatically can wrap the invocation in a shell retry loop (`while nexusctl watch; do sleep 1; done`), which matches how people already handle `journalctl -f` and similar streaming commands.

**Terminal not a TTY.** Interactive commands (pair, add-wifi with prompted PSK) need a TTY. If stdin/stdout aren't TTYs and the command would prompt, exit with code 5 and message "interactive prompt required but stdin is not a terminal (use --psk or set NEXUSCTL_PSK)".

**Operator cancels interactive flow.** Ctrl-C during pairing: nexusctl catches SIGINT, calls `fi.nexus.Bluetooth.CancelPairing`, waits for `PairingComplete` (with short timeout), prints "pairing cancelled", exits code 130 (standard SIGINT convention).

**Output stream closed.** stdout closed (e.g., `nexusctl iface list | head -1`). nexusctl handles EPIPE silently and exits code 0 — otherwise every piped invocation logs spurious errors. Standard behavior matching `ls`, `cat`, etc.

**Output too wide for terminal.** Tables in human mode need to fit. nexusctl detects terminal width via `crossterm::terminal::size` and truncates columns with an ellipsis. `--no-truncate` disables this. `--json` / `--terse` always emit full values.

**JSON output piped to non-JSON consumer.** Not detectable; operator's problem. We do document `--json` only produces valid JSON on stdout; all logs and prompts go to stderr.

**Invalid command arguments.** clap handles argument validation (types, required flags, mutually-exclusive groups). Exit code 2 for parse failures, matching POSIX convention.

**Config file missing or unreadable.** Non-fatal. nexusctl uses built-in defaults. Log at debug level if --verbose.

**Terminal signals during output rendering.** SIGWINCH (terminal resized) during a long list — just re-wrap on next page of output, or ignore if output is non-interactive. SIGTERM — exit cleanly.

Each failure mode has an explicit exit code (§4.3) and a predictable output shape. Shell scripts can rely on exit codes; operators get useful messages.

---

## 4. Command Structure

### 4.1 Command Tree

Commands follow a `<domain> <verb> [args]` pattern with short aliases where unambiguous. All commands accept the global options in §4.2.

```
nexusctl
├── status                            Show overall daemon status
│
├── iface                             Interface operations
│   ├── list [--kind <kind>]          List all interfaces
│   ├── show <iface>                  Show detailed interface state
│   └── events <iface>                Show recent events (last N)
│
├── eth                               Ethernet operations (profile-driven; use
│   │                                  `profile add-ethernet`/`remove` to enable
│   │                                  or disable 802.1X — there is no runtime
│   │                                  toggle, per DD-002 §3)
│   ├── list                          List Ethernet interfaces (alias: iface list --kind ethernet)
│   └── show <iface>                  Show Ethernet state, incl. 802.1X
│
├── wifi                              Wi-Fi operations
│   ├── list                          List Wi-Fi interfaces
│   ├── show <iface>                  Show current connection, signal, etc.
│   ├── scan [<iface>]                Trigger a scan; print results. If <iface>
│   │                                  is omitted and exactly one Wi-Fi
│   │                                  interface exists, uses it. With zero
│   │                                  interfaces: error (exit 1). With two or
│   │                                  more: error with a list (exit 2, usage).
│   ├── connect <ssid> [--iface X]    Connect by SSID (prompts for PSK if open/unknown)
│   ├── connect-profile <profile>     Connect using a stored profile (by ULID or SSID)
│   ├── disconnect [<iface>]          Disconnect
│   └── forget <ssid|ulid>            Remove a stored Wi-Fi profile
│
├── bt                                Bluetooth operations
│   ├── adapters                      List adapters
│   ├── power <hci> on|off            Set adapter power (property write on
│   │                                  fi.nexus.Bluetooth.Powered; DD-006 §6.4)
│   ├── scan [<hci>] [--duration N]   Discover devices. Adapter-selection
│   │                                  semantics match `wifi scan`: single
│   │                                  adapter implied when unambiguous,
│   │                                  otherwise error.
│   ├── list [--paired|--connected]   List known devices
│   ├── show <address>                Show device state
│   ├── pair <address>                Pair (interactive)
│   ├── connect <address>             Connect to a paired device
│   ├── disconnect <address>          Disconnect (keep bond)
│   ├── forget <address>              Remove bond
│   └── trust <address> on|off        Toggle trusted flag
│
├── gnss                              GNSS operations
│   ├── list                          List GNSS devices
│   ├── show [<device>]               Show current fix
│   └── satellites [<device>]         Show satellite table
│
├── profile                           Profile management
│   ├── list [--kind <kind>]          List profiles
│   ├── show <ulid|name>              Show a profile
│   ├── add-wifi <ssid> [opts]        Add a Wi-Fi profile (interactive PSK unless
│   │                                  --psk / NEXUSCTL_PSK set; --file PATH
│   │                                  reads a pre-built TOML, or '-' for stdin)
│   ├── add-ethernet <iface> [opts]   Add an Ethernet profile; --file / '-' as
│   │                                  with add-wifi
│   ├── import [--kind <kind>]        Read a TOML profile from stdin or --file
│   │                                  PATH and add it. --kind is required if
│   │                                  the profile's `kind` field is absent.
│   ├── edit <ulid>                   Edit in $EDITOR (see §6.5 for the
│   │                                  decrypt-in-tmpfs round-trip)
│   ├── remove <ulid|name>            Delete a profile
│   └── export <ulid>                 Dump profile TOML to stdout
│
├── power                             PowerState control
│   ├── get                           Print current PowerState
│   └── set <active|background|sleep> Change PowerState
│
├── admin                             Administrative operations
│   ├── rotate-master-key             Rotate the Profile Store master key
│   │                                  (Manager.RotateMasterKey; await
│   │                                  MasterKeyRotated signal for outcome)
│   ├── master-key-info               Show master-key source and status
│   │                                  (read Manager.MasterKeySource property
│   │                                  plus related read-only fields)
│   ├── reload-config                 Ask nexusd to re-read its config and
│   │                                  report what was applied vs deferred
│   │                                  (Manager.ReloadConfig; DD-006 §5.2)
│   ├── freeze-backup                 Acquire a backup lease (Manager.FreezeForBackup)
│   ├── release-backup <lease>        Release the lease (Manager.ReleaseBackupLease)
│   └── diagnostics [--out <path>]    Collect a support bundle
│                                      (Manager.CollectDiagnostics; writes to
│                                      stdout or --out)
│
├── watch                             Subscribe to D-Bus signals
│   ├── events                        All NotificationEvents
│   ├── iface                         Interface arrivals/departures
│   ├── wifi                          Wi-Fi state changes
│   ├── bt                            Bluetooth state/pairing events
│   └── gnss                          Fix updates
│
├── shell                             Interactive REPL (see §12 Phase 7 for detail;
│                                      mirrors connmanctl's dual one-shot/REPL
│                                      invocation model)
│
└── completions <bash|zsh|fish|pwsh>  Emit shell completion script
```

**Rationale for this layout.** Domains (`eth`, `wifi`, `bt`, `gnss`) match DD-006's D-Bus interface grouping. Within each, verbs follow nmcli conventions (`list`, `show`, `connect`, `disconnect`, `scan`). `profile` has its own tree because profiles span technologies. `admin` is separated because its operations aren't per-interface. `watch` is top-level because it's a fundamentally different mode of operation (long-running subscription vs one-shot call).

**Abbreviation rules.** `nexusctl wifi scan` is the full form; the client accepts any unambiguous prefix (`nexusctl w s` if the user is brave), matching nmcli's pattern. clap's `infer_subcommands(true)` handles this.

**No arguments.** `nexusctl` with no subcommand prints a one-screen summary: version, PowerState, a count of interfaces per kind, BlueZ/gpsd availability, and one-line status for each managed interface. This matches `connmanctl`'s first-screen output when a user runs the command to see "what's going on." It does *not* drop into a REPL — the REPL is available explicitly via `nexusctl shell`. Users who invoke `nexusctl` expecting help get the summary; users who specifically want the help menu pass `--help` or `-h`.

### 4.2 Global Options

These options apply to every subcommand and are parsed before the subcommand dispatch:

| Option | Default | Meaning |
|---|---|---|
| `--format <fmt>` / `-f` | `human` | Output format: `human`, `terse`, `json`, `pretty` (§5) |
| `--json` | — | Shorthand for `--format json` |
| `--terse` / `-t` | — | Shorthand for `--format terse` |
| `--pretty` | — | Shorthand for `--format pretty` |
| `--no-color` | auto | Disable ANSI colors; auto-detects TTY |
| `--color <when>` | `auto` | `always`, `auto`, `never` |
| `--timeout <s>` | per-command | Override D-Bus call timeout for this invocation |
| `--bus <address>` | system | D-Bus bus address (for test harnesses running on session bus) |
| `--verbose` / `-v` | — | Log D-Bus calls and signal subscriptions to stderr |
| `--quiet` / `-q` | — | Suppress non-essential output |
| `--no-interactive` | auto | Fail instead of prompting; auto-detects TTY |
| `--config <path>` | `~/.config/nexusctl/config.toml` | Alternative config file |

Behavior precedence: CLI flag → environment variable (`NEXUSCTL_FORMAT`, etc.) → config file → built-in default.

### 4.3 Exit Codes

Exit codes follow a deliberate scheme so shell scripts can distinguish error classes without parsing stderr.

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | General operation failure (the backend returned an error that doesn't fit another category) |
| 2 | Usage error (bad CLI arguments; emitted by clap) |
| 3 | Authorization denied (PolicyKit said no, or the bus refused the call) |
| 4 | Timeout |
| 5 | Interactive prompt required but no TTY available |
| 6 | nexusd unreachable (service not running, wrong bus) |
| 7 | Feature not enabled (reserved; see §9 — activated once DD-006 adds `fi.nexus.Error.FeatureDisabled`. Until then the CLI maps any such condition to exit 1 and a descriptive message) |
| 130 | SIGINT (Ctrl-C), following standard UNIX convention |

No other exit codes are used. If a command succeeds but produces a warning, exit code 0 with the warning on stderr.

---

## 5. Output Formats

nexusctl supports four output formats, selected per invocation via `--format`. Each has a distinct use case: human for interactive exploration, terse for shell scripts, JSON for programmatic parsing, pretty for verbose single-record display.

### 5.1 Default (Human)

Tabular, colored (if TTY + color allowed), with sensible column widths and right-sized truncation. A compact state-prefix column gives a connmanctl-style at-a-glance state indicator — useful when scanning a long interface or device list without reading every row's state column.

```
$ nexusctl iface list
    IFACE       KIND       STATE      MAC                 DETAILS
*O  eth0        ethernet   up         aa:bb:cc:dd:ee:01   1 Gbps, no auth
*AO eth1        ethernet   up         aa:bb:cc:dd:ee:02   1 Gbps, 802.1X authenticated
*AO wlan0       wireless   connected  aa:bb:cc:dd:ee:03   corp-net, -51 dBm, 5.2 GHz
*R  hci0        bluetooth  powered    dd:ee:ff:00:11:22   2 paired, 1 connected
*F  /dev/gps0   gnss       fix        —                   3D fix, 9 sats, 2.1m HDOP
    usb0        ethernet   down       aa:bb:cc:dd:ee:04   no carrier
```

The state-prefix codes (a subset of connmanctl's, adapted to Nexus's state machines):

| Code | Meaning | Applies to |
|---|---|---|
| `*` | Interface is present and enabled | all |
| `A` | Auto-configured / auto-connected (has a profile and it's in use) | ethernet, wifi, bluetooth |
| `O` | Online (carrier up, state is up/connected) | ethernet, wifi |
| `R` | Ready / powered but not actively connected | bluetooth adapters |
| `F` | Has a fix (GNSS) | gnss |
| `!` | Error state — see the STATE column for details | all |

**Per-kind derivation.** The prefix is computed from each kind's state machine:

| Kind | Present (`*`) | Active (`A`) | Online (`O`/`R`/`F`) | Error (`!`) |
|---|---|---|---|---|
| Ethernet | operstate ≠ `NotPresent` | profile attached (802.1X or plain) and backend considers it applied | `OperState::Up` with carrier | `AuthState::Failed` or analogous terminal-failure state |
| Wi-Fi | interface registered with backend | stored profile matched and `WifiState ∈ {Associating, Handshaking, Connected, Roaming}` | `WifiState::Connected` | `WifiState::Failed{..}` |
| Bluetooth (adapter) | BlueZ object present, `BtAdapterState` anywhere past `Unavailable` | at least one profiled device in `Connected` or `Connecting` on this adapter | `BtAdapterState::Powered` or `Discovering` and no `A` applies (shows as `R` for ready) | BlueZ unreachable for this adapter |
| Bluetooth (device) | seen in BlueZ's registry | profile present and `auto_connect = true` | `BtDeviceState::Connected` | `BtDeviceState::Failed{..}` |
| GNSS | gpsd sees the device | profile present and applied | current fix is 2D or 3D (`F`) | gpsd reports device error, or reader task terminally disconnected |

`!` takes precedence over every other letter when the error condition is true — a connected interface whose auth just failed renders as `*!O` briefly before transitioning, then the error dominates. Rows with no prefix (two-space padding) are interfaces Nexus knows about but hasn't activated — think of them as "known, not in use."

Single-record displays use a vertical layout:

```
$ nexusctl wifi show wlan0
Interface:   wlan0
State:       connected
BSSID:       aa:11:bb:22:cc:33
SSID:        corp-net
Frequency:   5.2 GHz
Signal:      -51 dBm (excellent)
Bitrate:     866 Mbps
Security:    WPA2-Personal
Profile:     01ARZ3NDEKTSV4RRFFQ69G5FAV
Connected:   2m 47s ago
```

Colors (when enabled):
- Green for healthy states (connected, up, authenticated, fix)
- Yellow for transient states (connecting, pairing, acquiring)
- Red for error states (failed, link-lost, auth-failed)
- Dim gray for missing/unavailable values

### 5.2 Terse

Shell-script-friendly. One record per line, fields separated by a configurable delimiter (default `:`), with a `--fields` option to select columns. No colors, no headers, no truncation.

```
$ nexusctl --terse iface list
eth0:ethernet:up:aa:bb:cc:dd:ee:01
eth1:ethernet:up:aa:bb:cc:dd:ee:02
wlan0:wireless:connected:aa:bb:cc:dd:ee:03
hci0:bluetooth:powered:dd:ee:ff:00:11:22
/dev/gps0:gnss:fix:

$ nexusctl --terse --fields=iface,state iface list
eth0:up
eth1:up
wlan0:connected
hci0:powered
/dev/gps0:fix

$ nexusctl --terse --separator='\t' --fields=iface,mac iface list
eth0	aa:bb:cc:dd:ee:01
eth1	aa:bb:cc:dd:ee:02
...
```

Rules for terse output:
- No headers.
- No color, ever.
- Fields in the order specified by `--fields`, or a sensible default per command.
- Empty fields are empty (not dashes or "—").
- Embedded separator characters in field values are escaped with a backslash. (Most fields can't contain `:` or tab, but Wi-Fi SSIDs can contain anything.)
- Record separator is newline.
- When only one field is requested, no separator is emitted — just the value. (Makes `$(nexusctl --terse --fields=state wifi show wlan0)` work as-is.)

### 5.3 JSON

Machine-parseable. Every command produces either a JSON object or array, matching a documented schema. The schema for each command is in §11 testing and generated as part of the CI.

```
$ nexusctl --json iface list
[
  {
    "iface": "eth0",
    "kind": "ethernet",
    "state": "up",
    "mac": "aa:bb:cc:dd:ee:01",
    "mtu": 1500,
    "carrier": true,
    "details": {
      "speed_mbps": 1000,
      "duplex": "full",
      "auth": null
    }
  },
  {
    "iface": "wlan0",
    "kind": "wireless",
    "state": "connected",
    "mac": "aa:bb:cc:dd:ee:03",
    "details": {
      "ssid": "corp-net",
      "bssid": "aa:11:bb:22:cc:33",
      "signal_dbm": -51,
      "frequency_mhz": 5180,
      "security": "wpa2-personal",
      "profile_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV"
    }
  }
]
```

Rules:
- Output is a single JSON value (object or array). No wrapping envelope.
- Field names are `snake_case`.
- Enums are lowercase strings (`"connected"`, not `"Connected"`).
- Timestamps are RFC 3339.
- Durations are integers in seconds (not ISO 8601; scripts want numbers).
- Missing / unavailable values are `null`.
- Byte-array fields (e.g., manufacturer data) are base64 strings with an explicit `*_b64` suffix on the field name.
- Errors in JSON mode write a JSON error object to stderr and exit non-zero:
  ```json
  {"error": "auth_failed", "message": "PolicyKit denied fi.nexus.profile.add"}
  ```

### 5.4 Pretty

Verbose multi-line format for a single record, designed for human reading when you want everything. Similar to `pretty` for git commits.

```
$ nexusctl --pretty bt show AA:BB:CC:DD:EE:FF
Device:       AA:BB:CC:DD:EE:FF
Adapter:      hci0 (on 00:11:22:33:44:55)
Name:         Bose SoundLink Revolve+
Alias:        Office Speaker
Transport:    dual (BR/EDR + LE)

State:        connected
Connected:    since 2025-11-02T14:37:02Z (47m ago)
Paired:       yes
Bonded:       yes
Trusted:      yes
Blocked:      no

RSSI:         -58 dBm
TX Power:     4 dBm (advertised)

Services (UUIDs):
  0000110b-0000-1000-8000-00805f9b34fb   Audio Sink
  0000111e-0000-1000-8000-00805f9b34fb   Handsfree
  0000110e-0000-1000-8000-00805f9b34fb   AV Remote Control

Profile:       /fi/nexus1/profile/bluetooth/01H9K2A7BZMFZG5N0J4SV4T3Q1
 Auto-connect: yes
 Auto-accept:  no
 Services:     (all authorized)
```

---

## 6. Interactive Flows

Three flows require terminal UI beyond "print this output": Bluetooth pairing, Wi-Fi passphrase entry, and PolicyKit authentication. Each has a precise sequence of D-Bus calls interleaved with terminal I/O, and each must handle operator cancellation cleanly.

### 6.1 Bluetooth Pairing

The most intricate flow. `nexusctl bt pair <address>` pairs a device, handling PIN/passkey/confirmation callbacks via the operator.

This is **the connmanctl agent pattern extended** to Bluetooth's richer callback surface. Where connmanctl's agent handles a single kind of prompt (network credentials), Bluetooth pairing has six (PIN, passkey entry, passkey display, numeric comparison, authorization, service authorization). The structural pattern is identical: backend drives, CLI responds, correlation via a job id.

```
$ nexusctl bt pair AA:BB:CC:DD:EE:FF
Pairing with AA:BB:CC:DD:EE:FF (Bose SoundLink Revolve+)...

Confirm this passkey matches the display on the peer device:

                    318 572

Do both sides show 318572? [y/N]: y

Pairing complete. Device is now trusted and auto-connect is enabled.
```

Sequence:

```
1.  Call fi.nexus.Bluetooth.Pair(device=AA:BB:CC:DD:EE:FF) → returns job_id

2.  Print "Pairing with <addr> (<name>)..." while awaiting the first signal.

3.  Subscribe to fi.nexus.Bluetooth.PairingPrompt filtered by job_id.
    Also subscribe to PairingComplete with the same filter.

4.  On PairingPrompt(kind, data):
    - request_confirmation: display the 6-digit passkey, prompt yes/no.
      Call AnswerPairingPrompt(job_id, answer: b:true/false).
    - request_passkey: prompt operator for 6 digits.
      Call AnswerPairingPrompt(job_id, answer: u:passkey).
    - request_pin: prompt operator for PIN string.
      Call AnswerPairingPrompt(job_id, answer: s:pin).
    - display_passkey / display_pin: show the value, say "enter this
      on the peer device", continue waiting. No response needed from
      the peer's perspective, but the backend's Agent method is
      blocked awaiting nexusctl's signal that the operator has seen
      the prompt; nexusctl calls AnswerPairingPrompt(job_id,
      answer: s:"acknowledge") to unblock it. This matches the
      AnswerPairingPrompt variant spec in DD-006 §6.4.
    - request_authorization: prompt yes/no for incoming connection.
      Call AnswerPairingPrompt(job_id, answer: b:true/false).
    - authorize_service: prompt yes/no with the service UUID.
      Call AnswerPairingPrompt(job_id, answer: b:true/false).

5.  On PairingComplete(job_id, success, reason):
    - success=true: print success message, exit 0.
    - success=false: print the reason in human form (e.g., "auth failed
      (PIN/passkey mismatch)"), exit 1.

6.  On SIGINT (Ctrl-C) before PairingComplete:
    - Call fi.nexus.Bluetooth.CancelPairing(device=<addr>).
    - Wait up to 2s for PairingComplete with reason=cancelled.
    - Print "pairing cancelled", exit 130.

    **Race resolution.** If PairingComplete arrives during the same
    poll cycle as the SIGINT — before or simultaneously with the
    CancelPairing round-trip — the outcome already on the wire wins:
    success reports "paired" and exits 0, a genuine failure reports
    its reason and exits 1. Cancellation only takes effect if nothing
    else has terminated the pairing first. This matches how similar
    tools (curl, git clone) treat "completed before the cancel
    arrived" — the operation wasn't actually cancelled, and claiming
    otherwise would be misleading.

7.  Overall timeout: nexusctl sets an outer timeout of `timeouts.pairing_s`
    (from config §8, default 90s) on the whole flow. If the backend hasn't
    emitted PairingComplete by then, call CancelPairing and exit with
    code 4.
```

Implementation notes:

- Use `tokio::select!` to multiplex the signal stream and terminal input. Don't block on `readline` while a signal might arrive.
- Render prompts on stderr so `--terse --json` still produce clean stdout (though interactive pairing with `--json` is usually a user error — nexusctl refuses with exit code 5 if stdin isn't a TTY).
- The passkey display block (large numbers, centered) is a visual cue that this is a security-critical moment. `dialoguer`'s prompt style isn't sufficient — use custom ANSI rendering.
- If the operator runs two `nexusctl bt pair` concurrently for different devices, each has a distinct job_id and signal filtering works.

**Testable factoring.** The terminal rendering and the flow logic are separated so the latter can be unit-tested without a PTY. The structure:

```rust
// In crates/nexus-client/src/interactive/pairing.rs:

/// Abstraction over the terminal — everything PairingFlow needs from
/// its environment. Implementations: TerminalPrompt (default, uses
/// stderr + dialoguer) and MockPrompt (deterministic responses driven
/// by a scripted Vec<PromptResponse> for tests).
#[async_trait]
pub trait Prompt: Send {
    async fn confirm(&mut self, text: &str, passkey: u32) -> Result<bool>;
    async fn ask_passkey(&mut self, text: &str) -> Result<u32>;
    async fn ask_pin(&mut self, text: &str) -> Result<String>;
    async fn acknowledge(&mut self, text: &str, passkey_or_pin: &str)
        -> Result<()>;
    async fn authorize(&mut self, text: &str, service_uuid: Option<&str>)
        -> Result<bool>;
    fn render_progress(&mut self, msg: &str);
    fn render_outcome(&mut self, ok: bool, detail: &str);
}

/// The pairing state machine. Consumes D-Bus signals and drives a
/// Prompt; produces a final PairingOutcome.
pub struct PairingFlow<P: Prompt> {
    prompt: P,
    bluetooth: BluetoothProxy<'_>,
    device: MacAddr,
    job_id: PairingJobId,
    overall_timeout: Duration,
}

impl<P: Prompt> PairingFlow<P> {
    pub async fn run(mut self) -> Result<PairingOutcome> { /* §6.1 steps 2-7 */ }
}

pub enum PairingOutcome {
    Paired,
    Failed { reason: String },
    Cancelled,
    TimedOut,
}
```

Unit tests construct a `PairingFlow<MockPrompt>` with scripted responses and a mock `BluetoothProxy` (using a synthetic signal stream), drive `run()`, and assert on the outcome. Every branch in §6.1 step 4 has a test. The production path substitutes `TerminalPrompt` and a real zbus proxy.

### 6.2 Wi-Fi Passphrase Entry

Simpler than pairing. When `nexusctl wifi connect corp-net` is called and no stored profile exists, nexusctl prompts for the PSK.

This follows the **connmanctl agent pattern** conceptually: the CLI registers itself (with Nexus, transiently for the duration of the command) as the responder for credential prompts, the backend requests credentials when wpa_supplicant needs them, and the CLI handles the interactive prompt. In nexusctl's case the "agent registration" is implicit — nexusctl subscribes to relevant signals for the pending connect operation and responds via D-Bus method — but the division of labor is the same: backend knows *when* credentials are needed, CLI knows *how* to ask for them on the operator's terminal.

```
$ nexusctl wifi connect corp-net
Security:  WPA2-Personal
Passphrase: ••••••••••••••
Save this network? [Y/n]: y
Profile name [corp-net]: corp-wifi

Connecting...
✓ Connected to corp-net (-48 dBm, 5.2 GHz)
```

Sequence:

1. If `--psk` or `NEXUSCTL_PSK` environment variable is set, use it directly.
2. Otherwise, scan (if not recent) to identify the AP's security type.
3. If security is open (no PSK), skip prompt.
4. If security is WPA2/WPA3-Personal, prompt for passphrase using `dialoguer::Password` (no echo).
5. Prompt "Save this network? [Y/n]" — if yes, also prompt for a profile name (default: SSID).
6. Call `fi.nexus.Manager.AddWifiProfile` (with save=true) or ephemeral connect (no save).
7. Call `fi.nexus.Wifi.Connect` with the profile/ephemeral config.
8. Subscribe to `StateChanged` on the Wi-Fi interface; wait for `connected` or `failed`.
9. Print result and exit.

Notes:

- WPA2/WPA3-Enterprise is not supported via interactive prompt (too many fields). Operators add those via `nexusctl profile add-wifi` with explicit flags, or via `--profile-file cert.toml`.
- If the backend rejects the PSK (auth failed), nexusctl re-prompts up to 3 times before giving up. Profile is saved only after a successful connection.
- **Credential-leak warning on `--psk`.** Passing a PSK as a command-line argument means it's visible in `ps` output (for the lifetime of the process) and in shell history. When `--psk` is used non-interactively, nexusctl prints a one-line warning to stderr: `warning: --psk passed on command line; prefer NEXUSCTL_PSK env var or an interactive prompt`. Suppress with `--no-warn-psk` (for scripts that handle the leak at a higher layer) or `NEXUSCTL_NO_WARN_PSK=1`. The warning does not affect exit code. Recommended alternatives: set `NEXUSCTL_PSK` (visible only to the process's own /proc/self/environ, not `ps`), pipe the PSK on stdin with `--psk-stdin`, or use an interactive prompt.

### 6.3 PolicyKit Authorization

Any mutating command may hit PolicyKit. nexusctl handles this transparently by spawning a `pkttyagent` subprocess for the duration of the call if no agent is registered.

```
$ nexusctl profile add-wifi corp-net --psk "$PSK"
==== AUTHENTICATING FOR fi.nexus.profile.add ====
Adding a Wi-Fi profile requires administrator privileges.
Authenticating as: alice
Password: ...
==== AUTHENTICATION COMPLETE ====
Profile added: 01H9K2A7BZMFZG5N0J4SV4T3Q1
```

Sequence:

1. Before any mutating command, nexusctl attempts to register as the authentication agent for its own process subject via `org.freedesktop.PolicyKit1.Authority.RegisterAuthenticationAgent`. PolicyKit's public API doesn't expose who's currently registered — an agent registration is private to the registrar — so nexusctl detects "another agent is already present" by the return of the register call itself: success means we own the agent for the duration; an error with `org.freedesktop.PolicyKit1.Error.Failed` (message `"An authentication agent already exists for the given subject"`) means some other agent (usually a desktop-session agent, or one the caller started themselves) is handling prompts, and nexusctl proceeds without spawning its own.
2. If step 1 succeeded and `auto_polkit_agent` config is true, fork-exec `pkttyagent --process $$` as a subprocess. It registers on the caller's behalf using the registration nexusctl just acquired.
3. Run the command normally. If PolicyKit prompts, `pkttyagent` handles the interaction on the terminal.
4. After the command completes, signal `pkttyagent` to exit and call `UnregisterAuthenticationAgent`.

If `pkttyagent` isn't installed (uncommon but possible in minimal environments), nexusctl warns once and proceeds — the command will still work if the operator is authenticated via some other mechanism (root, NOPASSWD sudo, etc.), otherwise it fails with fi.nexus.Error.AuthFailed.

Alternative: `--no-polkit-agent` skips the agent spawn, useful for scripts that handle auth upstream.

### 6.4 Signal Handling and Cancellation

Ctrl-C behavior differs by command:

| Command class | SIGINT behavior |
|---|---|
| Read-only (list, show, get) | Immediate exit (code 130) |
| Mutating (connect, pair, profile add) | Initiate graceful cancellation; exit within 2s |
| Long-running (watch, scan with --watch) | Clean exit immediately; no pending operation to cancel |
| Interactive pairing | Call CancelPairing, wait for PairingComplete briefly |

Multiple rapid Ctrl-C (within 500ms) forces immediate exit, skipping graceful cancellation. Matches curl, git's behavior.

### 6.5 Profile Edit Round-Trip

`nexusctl profile edit <ulid>` opens `$EDITOR` with the profile's TOML content. The round-trip needs care because profiles can contain credentials that, while encrypted on disk in the Profile Store, land in plaintext in the temporary file the editor uses.

Sequence:

1. Read the profile via `fi.nexus.Profile.Export()` (or the equivalent property read) — the daemon returns the full TOML, decrypted, as a string. This is the same path `nexusctl profile export` uses.
2. Create a temp file under a tmpfs-backed directory (`XDG_RUNTIME_DIR` if set — typically `/run/user/<uid>`, which is tmpfs on systemd systems — falling back to `$TMPDIR` then `/tmp`). The file mode is `0600` at creation via `O_CREAT | O_EXCL`. If no tmpfs path is available, nexusctl refuses the edit and prints a message explaining why (exit code 1) — falling back to disk-backed temp files risks plaintext credentials persisting on disk.
3. Write the profile TOML to the file. Pre-register a cleanup handler that will `shred -u` the file (overwrite + remove) on any exit path: normal, SIGTERM, SIGINT, panic.
4. Exec `$EDITOR` (or `$VISUAL`, then `vi` as fallback) on the temp file. Wait for it to exit.
5. If the editor exited non-zero or the file is unchanged (byte-identical to step 3), skip the write and run cleanup — no round-trip needed.
6. Otherwise, parse the edited TOML locally to validate it (catches syntax errors before a D-Bus round-trip). On parse failure, print the error and a "press Enter to re-open editor, or Ctrl-C to abandon" prompt, loop back to step 4.
7. Call `Profile.Update(settings)` with the parsed fields. The daemon re-validates and returns `InvalidArgument` on any disagreement — nexusctl surfaces that error and loops back to step 4, preserving the edited text.
8. On success, print confirmation and run cleanup.

**Cleanup is mandatory on every exit path.** A panic in clap or zbus must not leave plaintext credentials in the temp file. In Rust terms, the temp-file path is owned by a struct whose `Drop` runs the shred; the signal handler manually invokes `Drop` via `std::mem::forget` after shredding (or an equivalent explicit call) so the file is gone even if the process is being terminated.

If the operator wants a simpler non-interactive path — "set this one field and commit" — they use `nexusctl profile update <ulid> --field network.psk --value "$NEW_PSK"` (a separate command, shape mirrors nmcli) rather than the editor round-trip.

---

## 7. D-Bus Client Architecture

### 7.1 Connection

Connect to the system bus on startup. `zbus::Connection::system()` with tokio executor. The connection is used for the lifetime of the command (one invocation = one connection, no pooling).

For the `shell` REPL mode, the connection is held across command invocations.

If `--bus` is provided with a custom address (for test harnesses), use `zbus::Connection::for_address` instead.

### 7.2 Proxy Layer

Hand-written zbus proxies for each `fi.nexus.*` interface, generated from DD-006's definitions. Each proxy is a thin wrapper that:
- Constructs property and method calls with the correct signatures.
- Translates zbus errors to nexusctl's internal `Error` type (§9).
- Provides Rust-idiomatic return types (e.g., `Duration` for `duration_s: u32` dicts).

```rust
// In crates/nexus-client/src/proxy/wifi.rs:
#[zbus::proxy(
    interface = "fi.nexus.Wifi",
    default_service = "fi.nexus1"
)]
pub trait Wifi {
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn signal_dbm(&self) -> zbus::Result<i32>;

    fn scan(&self) -> zbus::Result<()>;

    fn connect(&self, profile: ObjectPath<'_>) -> zbus::Result<()>;

    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn state_changed(&self, state: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn scan_completed(&self) -> zbus::Result<()>;
}
```

The proxy module is the only place zbus types appear. Command handlers use the proxies, not raw zbus calls.

**Object-path resolution for per-interface proxies.** Most of Nexus's interface-scoped D-Bus objects (`fi.nexus.Wifi`, `fi.nexus.Bluetooth`, `fi.nexus.Gnss`, `fi.nexus.Ethernet`, `fi.nexus.BluetoothDevice`, the per-profile objects) live at paths like `/fi/nexus1/interface/<ifindex>` or `/fi/nexus1/profile/<kind>/<ulid>`. nexusctl receives human-friendly arguments (`wlan0`, `hci0`, `AA:BB:CC:DD:EE:FF`, or a profile label), so every handler that wants a per-interface proxy starts with a resolution step:

1. Call `Manager.GetInterface(ifname) -> (path: o)` for an ifname-keyed lookup, or
2. Walk `Manager.Interfaces` and match on a property read (used for Bluetooth device addresses, GNSS `gpsd_device` strings, etc. where no single-argument Manager method exists).

If the resolution step returns `fi.nexus.Error.NotFound`, nexusctl translates to `NexusctlError::NotFound { reference }` (exit code 1) with a hint listing what's actually present (e.g., "unknown interface `wlan9`; known Wi-Fi interfaces: wlan0"). This is the highest-volume error class and worth a dedicated friendly-message path — see §9.

Both resolution paths are cheap (single D-Bus round-trip for `GetInterface`, one round-trip plus filtering for the walk) so nexusctl does not cache paths across invocations. In `shell` REPL mode, the resolved path is cached for the duration of a single operation but not across commands — between commands the user may have reconfigured anything.

### 7.3 Signal Subscription

For commands that wait on a signal (pairing, connect), the proxy's generated `receive_<signal>()` method returns a `SignalStream`. Handlers use `tokio::select!` to multiplex signal arrival with timeouts and SIGINT.

```rust
// Sketch inside the pairing handler:
let mut prompt_stream = bluetooth.receive_pairing_prompt().await?;
let mut complete_stream = bluetooth.receive_pairing_complete().await?;
let ctrl_c = tokio::signal::ctrl_c();

loop {
    tokio::select! {
        Some(prompt) = prompt_stream.next() => {
            let p = prompt.args()?;
            if p.job_id != job_id { continue; }
            handle_prompt(p).await?;
        }
        Some(complete) = complete_stream.next() => {
            let c = complete.args()?;
            if c.job_id != job_id { continue; }
            return finalize(c).await;
        }
        _ = ctrl_c => {
            bluetooth.cancel_pairing(device).await?;
            // Fall through to wait briefly for PairingComplete(reason=cancelled)
        }
        _ = tokio::time::sleep(overall_timeout) => {
            bluetooth.cancel_pairing(device).await?;
            return Err(Error::Timeout);
        }
    }
}
```

### 7.4 The `watch` Command

Long-running subscription. Subscribes to one or more signals and prints them as they arrive. Output format respects `--format`.

```
$ nexusctl watch events
2025-11-02T14:37:02Z  interface-added   iface=eth0 kind=ethernet
2025-11-02T14:37:02Z  eth-auth-state    iface=eth0 state=authenticating
2025-11-02T14:37:03Z  eth-auth-state    iface=eth0 state=authenticated
2025-11-02T14:37:03Z  link-state        iface=eth0 state=up
2025-11-02T14:40:15Z  notification      kind=subsystem_unavailable subsystem=bluez duration_s=60

$ nexusctl --json watch events
{"time":"2025-11-02T14:37:02Z","kind":"interface-added","iface":"eth0","iface_kind":"ethernet"}
{"time":"2025-11-02T14:37:02Z","kind":"eth-auth-state","iface":"eth0","state":"authenticating"}
...
```

Rules:

- Each event is printed on its own line in terse/JSON mode. No batching. No buffering beyond what the OS does on stdout.
- In human mode, columns auto-align across events.
- `--filter <expr>` supports simple field matching. Multiple `--filter` flags AND together. Each filter is `<field>=<glob>` where `<glob>` is a shell-style wildcard (`*`, `?`, character classes). Supported fields are any key in the printed event dict: `kind`, `iface`, `state`, `address`, `subsystem`, etc. Examples: `--filter 'iface=eth0'`, `--filter 'kind=wifi-*'`, `--filter 'kind=link-state' --filter 'iface=eth?'`.
- `nexusctl watch` without a subcommand subscribes to *all* event kinds below.
- Specific subcommands (`nexusctl watch wifi`) subscribe only to the signals whose synthesized event kind starts with that domain prefix.
- Ctrl-C exits cleanly (code 0). No ack or draining.

**Signal synthesis.** nexusctl's `watch` output is a synthesis of multiple D-Bus signals, not a direct 1:1 projection. The CLI subscribes to the signals listed below and produces the named event kind:

| Event kind | Source D-Bus signal | Fields in output dict |
|---|---|---|
| `interface-added` | `org.freedesktop.DBus.ObjectManager.InterfacesAdded` on `/fi/nexus1`, filtered to `/fi/nexus1/interface/*` paths | `iface`, `iface_kind` (ethernet/wireless/bluetooth/gnss) |
| `interface-removed` | `ObjectManager.InterfacesRemoved` with matching path filter | `iface` |
| `link-state` | `fi.nexus.Interface.StateChanged` or the common `PropertiesChanged` on `OperState` | `iface`, `state` |
| `eth-auth-state` | `fi.nexus.Ethernet.AuthStateChanged` | `iface`, `state` |
| `wifi-state` | `fi.nexus.Wifi.StateChanged` | `iface`, `state` |
| `wifi-scan` | `fi.nexus.Wifi.ScanCompleted` | `iface`, `result_count` |
| `wifi-signal` | `PropertiesChanged` on `fi.nexus.Wifi.SignalDbm` (coalesced per DD-006 §9.3) | `iface`, `dbm` |
| `bt-adapter-state` | `fi.nexus.Bluetooth.StateChanged` | `adapter`, `state` |
| `bt-device-state` | `fi.nexus.BluetoothDevice.StateChanged` | `adapter`, `address`, `state` |
| `bt-pairing-started` | `fi.nexus.Bluetooth.PairingStarted` | `adapter`, `job_id`, `device` |
| `bt-pairing-prompt` | `fi.nexus.Bluetooth.PairingPrompt` | `adapter`, `job_id`, `kind`, plus kind-specific fields (passkey/pin/service_uuid) |
| `bt-pairing-complete` | `fi.nexus.Bluetooth.PairingComplete` | `adapter`, `job_id`, `success`, `reason` |
| `gnss-fix` | `fi.nexus.Gnss.FixChanged` | `device`, `mode`, `lat`, `lon`, plus optional `alt`, `hdop` |
| `profile-changed` | `ObjectManager.InterfacesAdded`/`InterfacesRemoved` filtered to `/fi/nexus1/profile/*` | `kind`, `action` (added/removed), `id` |
| `notification` | `fi.nexus.Manager.NotificationEvent` | `kind` (pass-through, e.g., `credentials_invalid`), plus every key from the signal's `data` dict |
| `master-key-rotated` | `fi.nexus.Manager.MasterKeyRotated` | `job_id`, `outcome`, `profiles_rewritten`, `duration_ms` |
| `power-state` | `fi.nexus.Manager.PowerStateChanged` | `state` |

Subscribing a subset via a `watch` subcommand maps as follows: `iface` → `interface-*` and `link-state`; `wifi` → `wifi-*`; `bt` → `bt-*`; `gnss` → `gnss-*`. The catch-all `events` subcommand subscribes to every row above.

**Event dicts are flat**, not nested, to keep `--filter` simple and terse-mode output readable. Nested fields from D-Bus (e.g., `NotificationEvent.data` is `a{sv}`) are flattened into the top-level event dict with the same keys.

---

## 8. Configuration

nexusctl has a small config file for user defaults:

```toml
# ~/.config/nexusctl/config.toml

[output]
# Default format when --format isn't specified.
format = "human"       # "human" | "terse" | "json" | "pretty"
color = "auto"         # "auto" | "always" | "never"

[terse]
# Separator for --terse output.
separator = ":"

[timeouts]
# Per-command-class D-Bus call timeout in seconds.
default_s = 10
scan_s = 30
pairing_s = 90
connect_s = 45

[interactive]
# Whether to spawn pkttyagent automatically.
auto_polkit_agent = true
# Maximum Wi-Fi PSK retry prompts before giving up.
max_psk_retries = 3

[watch]
# Default format for `nexusctl watch`. Overrides [output.format].
format = "human"
# Maximum seconds to hold a signal subscription before graceful exit
# (0 = no limit).
max_duration_s = 0
```

CLI flags override config; config overrides built-in defaults.

Environment variables (subset):

- `NEXUSCTL_FORMAT` — overrides `[output.format]`
- `NEXUSCTL_TIMEOUT` — overrides per-call timeouts
- `NEXUSCTL_PSK` — Wi-Fi PSK for non-interactive use
- `NEXUSCTL_NO_POLKIT_AGENT=1` — equivalent to `--no-polkit-agent`
- `NEXUSCTL_NO_WARN_PSK=1` — suppress the credential-leak warning when `--psk` is used on the command line (see §6.2)

System-wide config at `/etc/nexusctl/config.toml` is consulted as a fallback if the user's file is absent. CLI → env → user config → system config → built-in.

---

## 9. Error Translation

D-Bus errors arrive as `zbus::Error::MethodError(name, Some(message), _)`. nexusctl translates these to human messages and appropriate exit codes.

```rust
enum NexusctlError {
    AuthDenied { action: String, hint: String },           // exit 3
    Timeout { operation: String, duration: Duration },     // exit 4
    NotInteractive { operation: String },                  // exit 5
    NexusdUnreachable,                                     // exit 6
    FeatureDisabled { feature: String },                   // exit 7
    InvalidState { operation: String, state: String },     // exit 1
    InvalidArgument { message: String },                   // exit 1
    NotFound { reference: String },                        // exit 1
    AlreadyExists { reference: String },                   // exit 1
    UnknownDevice { address: String },                     // exit 1
    UnknownPairingJob { job_id: String },                  // exit 1
    ConnectionFailed { reason: String },                   // exit 1
    BluezUnavailable,                                      // exit 1
    SupplicantUnavailable,                                 // exit 1
    NotPowered { adapter: String },                        // exit 1
    NotPaired { device: String },                          // exit 1
    ResourceBusy { resource: String },                     // exit 1
    IoError { detail: String },                            // exit 1
    CryptoError { detail: String },                        // exit 1
    Unsupported { detail: String },                        // exit 1
    Other { raw: String },                                 // exit 1
}
```

Translation table (matching the canonical error list in DD-006):

| D-Bus error | nexusctl Error | Exit | Human message |
|---|---|---|---|
| `fi.nexus.Error.AuthFailed` | `AuthDenied` | 3 | "permission denied: `<action>` requires `<hint>`" |
| `fi.nexus.Error.InvalidState` | `InvalidState` | 1 | "operation not valid in current state: `<state>`" |
| `fi.nexus.Error.InvalidArgument` | `InvalidArgument` | 1 | "invalid argument: `<message>`" |
| `fi.nexus.Error.NotFound` | `NotFound` | 1 | "not found: `<reference>`" |
| `fi.nexus.Error.AlreadyExists` | `AlreadyExists` | 1 | "already exists: `<reference>`" |
| `fi.nexus.Error.UnknownDevice` | `UnknownDevice` | 1 | "device not known: `<address>`" |
| `fi.nexus.Error.UnknownPairingJob` | `UnknownPairingJob` | 1 | "no pairing with job id `<job_id>`" |
| `fi.nexus.Error.ConnectionFailed` | `ConnectionFailed` | 1 | "connection failed: `<reason>`" |
| `fi.nexus.Error.BluezUnavailable` | `BluezUnavailable` | 1 | "BlueZ is not running or not reachable" |
| `fi.nexus.Error.SupplicantUnavailable` | `SupplicantUnavailable` | 1 | "wpa_supplicant/iwd is not available" |
| `fi.nexus.Error.NotPowered` | `NotPowered` | 1 | "adapter `<hci>` is not powered" |
| `fi.nexus.Error.NotPaired` | `NotPaired` | 1 | "device `<address>` is not paired" |
| `fi.nexus.Error.ResourceBusy` | `ResourceBusy` | 1 | "resource busy: `<resource>`" |
| `fi.nexus.Error.Timeout` | `Timeout` | 4 | "operation timed out after `<s>`s" |
| `fi.nexus.Error.IoError` | `IoError` | 1 | "I/O error: `<detail>`" |
| `fi.nexus.Error.CryptoError` | `CryptoError` | 1 | "crypto error: `<detail>`" |
| `fi.nexus.Error.Unsupported` | `Unsupported` | 1 | "unsupported: `<detail>`" |
| `org.freedesktop.DBus.Error.ServiceUnknown` | `NexusdUnreachable` | 6 | "nexusd is not running (try `systemctl start nexus`)" |
| `org.freedesktop.DBus.Error.NoReply` | `Timeout` | 4 | "D-Bus timeout: no reply from nexusd" |
| `org.freedesktop.DBus.Error.AccessDenied` | `AuthDenied` | 3 | "D-Bus access denied (is nexus.conf policy installed?)" |

**Errors this DD assumes and flags for addition to DD-006.** nexusctl references the following errors that aren't yet in DD-006's canonical list. They're small additions that would need to land in DD-006 before implementation:

| Missing from DD-006 | Suggested use |
|---|---|
| `fi.nexus.Error.RateLimited` | Per-sender rate limits (DD-006 §11 describes rate limiting but doesn't name the error) |
| `fi.nexus.Error.FeatureDisabled` | When nexusd has a backend disabled in config and a client calls a method on the missing interface |

Until added, nexusctl maps these cases to `Other` with a best-effort message.

**Pairing failures** surface via the `PairingComplete` signal with `success=false, reason=<string>`, not via a D-Bus error on the `Pair()` method. `Pair()` itself returns success when the pairing *starts*; the outcome arrives asynchronously. nexusctl's interactive flow (§6.1) handles this directly — no D-Bus-error translation needed for pairing outcomes.

**Not-connected** states are surfaced via `InvalidState`, not a separate error. nexusctl's error message distinguishes by context ("not connected" vs "not in the right state for this operation").

In `--json` mode, errors become a JSON object on stderr:

```json
{
  "error": "auth_denied",
  "action": "fi.nexus.profile.add",
  "message": "permission denied: adding a profile requires admin privileges",
  "hint": "authenticate as a member of the nexus-admin group"
}
```

---

## 10. Shell Completion

clap's derive API plus `clap_complete` generates completion scripts for bash, zsh, fish, and PowerShell.

```
$ nexusctl completions bash > /etc/bash_completion.d/nexusctl
$ nexusctl completions zsh > ~/.zfunc/_nexusctl
$ nexusctl completions fish > ~/.config/fish/completions/nexusctl.fish
```

**Per-distro install targets** (for packagers):

| Shell | System-wide path | User path |
|---|---|---|
| bash | `/etc/bash_completion.d/nexusctl` (Debian) or `/usr/share/bash-completion/completions/nexusctl` (Fedora/Arch) | `~/.local/share/bash-completion/completions/nexusctl` |
| zsh | `/usr/share/zsh/site-functions/_nexusctl` | `~/.zfunc/_nexusctl` (requires `fpath+=~/.zfunc` in `.zshrc`) |
| fish | `/usr/share/fish/vendor_completions.d/nexusctl.fish` | `~/.config/fish/completions/nexusctl.fish` |
| PowerShell | profile include | user profile include |

A future `nexusctl install-completions [--system]` helper is anticipated but not in scope for v0.1 — packagers currently install by redirecting `nexusctl completions <shell>` to the appropriate path in their package's post-install script.

Beyond static command completion, nexusctl provides dynamic completion for arguments that depend on runtime state:

- `<iface>` → completes to current interface names from `nexusctl --terse --fields=iface iface list`
- `<address>` (Bluetooth) → completes to known device addresses
- `<ssid>` (Wi-Fi connect) → completes to recent scan results + stored profiles
- `<ulid|name>` (profile ops) → completes to profile labels and ULIDs

Dynamic completion uses clap's `ValueHint` and `value_parser` with a custom `fn` that queries D-Bus. To avoid slow completion on slow systems, the query has a 1 s hard timeout — if it doesn't return, completion falls back to static alternatives only.

---

## 11. Testing Strategy

### 11.1 Unit Tests

- **Argument parsing.** `clap` parse tests for every subcommand + flag combination. Ensures renames don't break existing invocations.
- **Output format.** Snapshot tests (via `insta`) for each format (human, terse, JSON, pretty) against fixture data. Changes to output format are deliberate code review, not accidental drift.
- **Error translation.** Every row in the §9 error translation table has a test that passes in a mock D-Bus error and verifies the nexusctl error, exit code, and message.
- **Field selection in `--terse`.** All `--fields` combinations produce expected output.

### 11.2 Integration Tests

- **Mock nexusd.** A test harness spawns a zbus ObjectServer registering `fi.nexus1` on a session bus, handles configurable mock responses, and verifies that nexusctl commands produce the expected output and exit code.
- **PolicyKit denial.** Mock D-Bus returns `AuthFailed`; verify exit code 3 and correct message.
- **Service-unknown.** Mock D-Bus returns `ServiceUnknown`; verify exit code 6.
- **Signal subscription.** Mock fires signals; verify `nexusctl watch` prints them in the expected format.

### 11.3 End-to-End Tests

Against a real nexusd running on a test VM:
- `nexusctl iface list` matches the interfaces `ip link show` reports (modulo loopback, BT, GNSS).
- `nexusctl wifi scan` returns results matching `iwlist wlan0 scan`.
- `nexusctl bt adapters` matches `bluetoothctl list`.

These are gated behind `#[cfg(feature = "integration-e2e")]` and run in a dedicated VM in CI.

### 11.4 Interactive Flow Tests

The pairing and passphrase flows are hardest to test. Two approaches:
- **`expect`-style harness** — drives nexusctl's stdin/stdout programmatically. Requires careful handling of ANSI escapes.
- **Factor out the flow logic from the terminal I/O** — put pairing logic in a `PairingFlow` struct parameterized by a `trait Prompt` (with a mock impl for tests). Most of the testable behavior lives in the logic; the terminal rendering is a thin shell.

The second approach is preferred.

---

## 12. Implementation Phases

### Phase 1 — Skeleton, connection, `status` and `iface list`

Bootstrap the crate. Parse args with clap. Connect to D-Bus. Implement `nexusctl status` and `nexusctl iface list` in human and JSON formats.

**Exit criterion:** `nexusctl status` against a running nexusd prints the daemon version and interface count. `nexusctl --json iface list` outputs a valid JSON array.

### Phase 2 — Output formats and error translation

Add `--terse`, `--pretty`, `--format`. Implement the full error translation table from §9. All four formats work for `iface list` and `iface show`. Exit codes match §4.3.

**Exit criterion:** All four format modes produce correct output per snapshot tests. Every error in §9 has a passing test.

### Phase 3 — Read-only subcommands for every domain

`iface`, `eth list`, `wifi list/show/scan`, `bt adapters/list/show`, `gnss list/show`, `profile list/show`. No mutating operations yet.

**Exit criterion:** Every read-only command from §4.1 works against a real nexusd. All commands produce output in all four formats.

### Phase 4 — Mutating commands (non-interactive path)

`wifi connect` (with `--psk` non-interactive), `wifi disconnect`, `bt power`, `bt connect`, `bt disconnect`, `bt forget`, `bt trust`, `profile add-wifi` (with `--psk`), `profile remove`, `power set`.

**Exit criterion:** Every non-interactive mutating command works. PolicyKit denial produces the correct error and exit code.

### Phase 5 — Interactive flows

Wi-Fi passphrase prompt, Bluetooth pairing (all Agent callback kinds), PolicyKit agent spawning. Full cancellation semantics (§6.4).

**Exit criterion:** Pairing against a real device with numeric comparison works. Wi-Fi connect with prompted PSK works. Ctrl-C mid-pair invokes CancelPairing and exits 130.

### Phase 6 — `watch` and signal subscription

`nexusctl watch` with filters. `scan --watch` for continuous updates.

**Exit criterion:** `nexusctl watch events` prints all NexusEvent flavors as they fire. `--filter` correctly narrows.

### Phase 7 — `shell` REPL

Interactive REPL using `rustyline`. Holds a single D-Bus connection across commands. Tab completion inside the REPL.

**Exit criterion:** `nexusctl shell` drops into a prompt, accepts any valid one-shot command, maintains history between invocations.

### Phase 8 — Shell completion and polish

Static + dynamic completion for bash/zsh/fish/pwsh. Man pages. Integration tests end-to-end in a CI VM.

**Exit criterion:** `nexusctl completions bash | bash -n` parses cleanly. Tab-completing an interface name in bash produces correct suggestions. E2E tests pass in CI.

---

## Related Documents

- [Nexus Architecture](./nexus-architecture.md) — Parent
- [DD-006: D-Bus API](./dd-006-dbus-api.md) — The sole dependency. Every nexusctl command maps to one or more D-Bus calls or signal subscriptions defined in DD-006. When DD-006 adds a method, this DD adds a subcommand to match.
- [DESIGN-DOCS.md](./DESIGN-DOCS.md) — Conventions this DD follows (Pass 4 failure-mode-first, §5 output format as design, command-driven structure)
- [CLAUDE-CODE-PROMPTS.md](./CLAUDE-CODE-PROMPTS.md) — Implementation prompt series. A future prompt for nexusctl would slot in after the main Nexus daemon is running and DD-006 Phases 1-3 are done.
