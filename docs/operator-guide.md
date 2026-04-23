# Nexus Operator Guide

Nexus is the platform connectivity manager for embedded Linux
devices. This guide covers installation, day-to-day operation,
and troubleshooting. Architecture details live in
[`nexus-architecture.md`](../nexus-architecture.md); per-subsystem
designs are in the `dd-*` detailed design documents at the repo
root.

## 1. Prerequisites

Nexus targets Linux with:

- **systemd** — both as the service manager and for the delegated
  IP layer (`systemd-networkd`).
- **D-Bus** — the system bus.
- **PolicyKit** — enforces authorization on mutating D-Bus methods
  (DD-006 §10).
- **wpa_supplicant** — when Wi-Fi is enabled (default supplicant;
  see [ADR-002](../nexus-architecture.md#43-key-architectural-decisions)).
- **BlueZ** — when Bluetooth is enabled.
- **gpsd** — when GNSS is enabled.

A minimum-deps install (just Ethernet + discovery) only needs
systemd, D-Bus, and PolicyKit.

## 2. Install

From a source checkout, build a release binary and run the
installer:

```bash
cargo build --release -p nexus-daemon
sudo packaging/nexus-install.sh
```

This installs:

| Path                                                | Content                              |
|-----------------------------------------------------|--------------------------------------|
| `/usr/local/bin/nexusd`                             | Daemon binary                        |
| `/etc/nexus/nexus.toml`                             | Main config                          |
| `/etc/nexus/examples/*.conf`                        | Example drop-in snippets             |
| `/lib/systemd/system/nexus.service`                 | systemd unit                         |
| `/usr/lib/tmpfiles.d/nexus.conf`                    | Creates `/var/lib/nexus`             |
| `/usr/share/polkit-1/actions/fi.nexus.policy`       | PolicyKit action definitions         |
| `/etc/polkit-1/rules.d/50-nexus.rules`              | Example JS authorization rules       |

It creates three system groups:

- `nexus` — the service account the daemon runs as.
- `nexus-admin` — full administrative access (profile writes,
  master-key rotation, diagnostics).
- `nexus-user` — can scan and (dis)connect existing profiles.

Add your operator accounts to the appropriate group:

```bash
sudo usermod -aG nexus-admin alice
# alice must log out and back in for group changes to take effect.
```

## 3. Start the daemon

After reviewing `/etc/nexus/nexus.toml`:

```bash
sudo systemctl start nexus
sudo systemctl status nexus
```

The service uses `Type=notify`: `systemctl start` returns only
once the daemon has finished spawning every enabled subsystem and
has issued `READY=1` via the notify socket.

Enable at boot:

```bash
sudo systemctl enable nexus
```

The install script already does this.

## 4. Verify

Nexus publishes itself on the system bus as `fi.nexus1` at
`/fi/nexus1`. Probe it with `busctl`:

```bash
busctl introspect fi.nexus1 /fi/nexus1
busctl call fi.nexus1 /fi/nexus1 \
    org.freedesktop.DBus.ObjectManager GetManagedObjects
```

Expected output: the `Manager` object plus one `Interface` object
per physical NIC discovered by the Interface Monitor.

## 5. Day-to-day operation

| Task                                    | Command                                                  |
|-----------------------------------------|----------------------------------------------------------|
| Read logs (follow)                      | `journalctl -u nexus -f`                                 |
| Read logs (last 200 lines)              | `journalctl -u nexus -n 200`                             |
| Restart after config change             | `sudo systemctl restart nexus`                           |
| Stop                                    | `sudo systemctl stop nexus`                              |
| Tail interface-discovery events         | `busctl --match "type='signal',interface='org.freedesktop.DBus.ObjectManager'"` |
| List Wi-Fi profiles (if any)            | `busctl tree fi.nexus1`                                  |

Config changes require a restart — Nexus does not hot-reload
`nexus.toml`. Runtime state in `/var/lib/nexus` is preserved
across restarts.

## 6. Logging

Nexus uses `tracing` structured logging, routed to stdout /
stderr by default. Under systemd that goes straight to the
journal, where you can filter by level:

```bash
journalctl -u nexus -p warning..  # warn + error
journalctl -u nexus -p err..      # error only
```

Log level is set in `nexus.toml` (`log_level = "info"`). The
`RUST_LOG` environment variable, when set by a systemd drop-in,
overrides it using standard `tracing-subscriber` filter syntax
(e.g., `nexus_wifi=debug,info`).

## 7. Troubleshooting

**The service starts but `systemctl status` reports "activating"
forever.**
`Type=notify` is set; the daemon either crashed before issuing
`READY=1` or the `NOTIFY_SOCKET` is unreachable. Check
`journalctl -u nexus` for errors during startup — most commonly
a missing `/var/lib/nexus` (tmpfiles was not run) or a broken
master-key path.

**Mutating D-Bus methods return `fi.nexus.Error.AuthFailed`.**
PolicyKit is rejecting the call. Either the caller is not in
`nexus-admin` / `nexus-user`, or the rules file was not installed
(`pkaction --action-id fi.nexus.scan` should list an entry).

**Wi-Fi is enabled in config but never scans.**
`wpa_supplicant.service` is probably not running. `systemctl
status wpa_supplicant` and `busctl list | grep wpa` will confirm.

**The journal shows `subsystem crashed` messages.**
Supervision is doing its job — the subsystem is restarting with
exponential backoff. Look at the `error=` field to understand
why. If the backoff cap is being hit (default 30 s), consider
disabling that subsystem until the underlying cause is fixed
(e.g., BlueZ not installed, gpsd endpoint unreachable).

## 8. Uninstall

```bash
sudo systemctl disable --now nexus
sudo rm -f /usr/local/bin/nexusd
sudo rm -f /lib/systemd/system/nexus.service
sudo rm -f /usr/lib/tmpfiles.d/nexus.conf
sudo rm -f /usr/share/polkit-1/actions/fi.nexus.policy
sudo rm -f /etc/polkit-1/rules.d/50-nexus.rules
sudo systemctl daemon-reload
# Keep or remove /etc/nexus and /var/lib/nexus as you prefer;
# they contain operator config and profile data respectively.
```

## 9. Next steps

- Full configuration reference: [`configuration.md`](./configuration.md).
- Architecture + ADRs: [`../nexus-architecture.md`](../nexus-architecture.md).
- D-Bus API: [`../dd-006-dbus-api.md`](../dd-006-dbus-api.md).
