# Nexus Configuration Reference

Nexus reads a single TOML file at startup, defaulting to
`/etc/nexus/nexus.toml` (override with `nexusd --config PATH`).
Unknown keys are rejected; missing sections fall back to their
per-section defaults.

This document lists every recognized field. Cross-references
link to the detailed-design sections where the semantics are
defined. Skim the [operator guide](./operator-guide.md) first for
installation flow.

---

## Top-level

| Field          | Type     | Default   | Notes                                                                |
|----------------|----------|-----------|----------------------------------------------------------------------|
| `bus_capacity` | integer  | `256`     | Capacity of the `NexusEvent` broadcast channel (architecture doc §6). Must be > 0. Bump up only if journal shows `event bus receiver lagged` at INFO/WARN. |
| `log_level`    | string   | `"info"`  | `error` / `warn` / `info` / `debug` / `trace`. `RUST_LOG` overrides. |

---

## `[supervision]`

Governs how subsystems are restarted on crash (architecture doc §5).

| Field                     | Type    | Default | Notes                                                     |
|---------------------------|---------|---------|-----------------------------------------------------------|
| `restart`                 | bool    | `true`  | If false, a crashed subsystem stays down until restart.   |
| `restart_initial_backoff` | seconds | `0.5`   | Delay before the first restart attempt.                   |
| `restart_max_backoff`     | seconds | `30.0`  | Cap on the exponential backoff.                           |
| `restart_multiplier`      | float   | `2.0`   | Multiplier between successive backoffs. Must be ≥ 1.0.    |

---

## `[interface_monitor]`

The unified discovery layer. See
[`dd-001-interface-discovery.md`](../dd-001-interface-discovery.md).

| Field     | Type | Default | Notes                                                  |
|-----------|------|---------|--------------------------------------------------------|
| `enabled` | bool | `true`  | Must stay enabled in practice — every backend depends on it. Disabling is useful for purely D-Bus-introspection tests. |

---

## `[profile_store]`

Encrypted on-disk profile storage. See
[`dd-007-profile-store.md`](../dd-007-profile-store.md).

| Field            | Type          | Default                                 | Notes                                                                                                                                                                                                 |
|------------------|---------------|-----------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `root`           | string (path) | `/var/lib/nexus`                        | Root directory. The systemd unit's `ReadWritePaths=` must include this.                                                                                                                               |
| `key_source`     | string        | `"file"`                                | One of `"file"` (random key at `<root>/keys/master.key`; dev/embedded default), `"in_memory"` (derived from `in_memory_seed`; test-only), or `"tpm"` (requires a build with the `tpm` Cargo feature). |
| `in_memory_seed` | hex string    | —                                       | 64 lowercase hex chars = 32 bytes. Required when `key_source = "in_memory"`; ignored otherwise. **Do not commit production seeds.**                                                                    |

---

## `[dbus]`

External D-Bus API. See
[`dd-006-dbus-api.md`](../dd-006-dbus-api.md).

| Field                              | Type   | Default       | Notes                                                                                                   |
|------------------------------------|--------|---------------|---------------------------------------------------------------------------------------------------------|
| `enabled`                          | bool   | `true`        | Disables the D-Bus service layer entirely. Usable only for offline tests.                                |
| `bus_name`                         | string | `"fi.nexus1"` | Well-known name to request. Must not be empty.                                                           |
| `use_session_bus`                  | bool   | `false`       | `true` → session bus (dev only). `false` → system bus.                                                   |
| `address`                          | string | —             | Explicit bus address. When set, overrides `use_session_bus`. Used by integration tests with a private `dbus-daemon`. |
| `allow_all_authz`                  | bool   | `false`       | When `true`, every PolicyKit check is skipped. **Development only.**                                     |
| `rate_limit_property_read_per_min` | int    | `1000`        | Per-sender limit (DD-006 §15). Over-cap calls return `fi.nexus.Error.ResourceBusy`.                      |
| `rate_limit_scan_per_min`          | int    | `10`          |                                                                                                         |
| `rate_limit_connect_per_min`       | int    | `30`          | Applies to both `Connect` and `Disconnect`.                                                              |
| `rate_limit_profile_write_per_min` | int    | `30`          | Covers `AddWifiProfile`, `AddEthernetProfile`, `RemoveProfile`, and `Profile.Update`.                    |
| `rate_limit_admin_per_min`         | int    | `1`           | Admin ops: `RotateMasterKey`, `FreezeForBackup`, `ReleaseBackupLease`, `ClearQuarantine`.                |

### Authorization

If `allow_all_authz = false`, the daemon instantiates a
`PolicyKitChecker` against a dedicated system-bus connection.
Callers must be granted access via `/etc/polkit-1/rules.d/*.rules`
(see `packaging/polkit-1/rules.d/50-nexus.rules` for an example).
The action IDs are:

- `fi.nexus.read` — default YES.
- `fi.nexus.scan` — default `auth_self_keep`.
- `fi.nexus.connect` — default `auth_self_keep`.
- `fi.nexus.profile.add` — default `auth_admin_keep`.
- `fi.nexus.profile.modify` — default `auth_admin_keep`.
- `fi.nexus.profile.read_credentials` — default NO.
- `fi.nexus.set_power` — default `auth_self_keep`.
- `fi.nexus.admin` — default `auth_admin_keep`.

---

## `[ethernet]`

Wired carrier tracking + optional 802.1X. See
[`dd-002-ethernet-backend.md`](../dd-002-ethernet-backend.md).

| Field                | Type    | Default           | Notes                                                                                         |
|----------------------|---------|-------------------|-----------------------------------------------------------------------------------------------|
| `enabled`            | bool    | `true`            |                                                                                               |
| `auth_backend`       | string  | `"wpa_supplicant"`| One of `"wpa_supplicant"`, `"ead"`, `"none"`. `none` disables wired 802.1X entirely.          |
| `retry_initial`      | seconds | `0.5`             | Initial backoff after an auth failure.                                                        |
| `retry_max`          | seconds | `30.0`            | Cap on the exponential backoff.                                                               |
| `retry_multiplier`   | float   | `2.0`             | Multiplier between successive backoffs.                                                       |
| `retry_max_attempts` | int     | `8`               | After this many failures the interface is held in `Failed` until operator intervention.       |

---

## `[wifi]`

Wi-Fi state machine driven by a pluggable supplicant. See
[`dd-003-wifi-backend.md`](../dd-003-wifi-backend.md) and
[ADR-002](../nexus-architecture.md#43-key-architectural-decisions).

| Field                       | Type    | Default           | Notes                                                                                                     |
|-----------------------------|---------|-------------------|-----------------------------------------------------------------------------------------------------------|
| `enabled`                   | bool    | `true`            | Disabling saves ~everything Wi-Fi-related on Ethernet-only devices.                                       |
| `backend`                   | string  | `"wpa_supplicant"`| `"wpa_supplicant"` (default; certified path), `"iwd"` (integrator must validate), or `"mock"` (tests).    |
| `roam_mode`                 | string  | `"supplicant"`    | `"off"`, `"supplicant"`, or `"nexus"`. Policies differ — see DD-003 §7.                                   |
| `signal_poll_interval`      | seconds | `5.0`             | Cadence for `SignalPoll` while associated.                                                                |
| `disconnect_cool_down`      | seconds | `2.0`             | Minimum interval between consecutive connect attempts to the same BSS.                                    |
| `supplicant_event_capacity` | int     | `128`             | Internal `SupplicantEvent` broadcast capacity. Bump up on chipsets with very noisy state transitions.     |

---

## `[bluetooth]`

Bluetooth Classic + BLE via BlueZ. See
[`dd-004-bluetooth-backend.md`](../dd-004-bluetooth-backend.md).

| Field                     | Type | Default | Notes                                                                                                                                           |
|---------------------------|------|---------|-------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`                 | bool | `true`  | Requires `bluetooth.service` (BlueZ) to be installed and reachable.                                                                              |
| `mock`                    | bool | `false` | Use the in-process mock BlueZ client. Test / driver bring-up only.                                                                               |
| `pairing_timeout_s`       | int  | `60`    |                                                                                                                                                 |
| `agent_response_timeout_s`| int  | `45`    | Per DD-006 §5.3 — how long an operator has to answer a pairing prompt before the backend declares it failed.                                     |
| `discovery_timeout_s`     | int  | `30`    | Default discovery window; `SetDiscoveryFilter` on the D-Bus API overrides this per-call.                                                         |
| `discovery_device_ttl_s`  | int  | `300`   | Age at which a device heard during discovery but not since is evicted.                                                                           |
| `bluez_outage_notify_s`   | int  | `60`    | Emit `OperatorNotification` if BlueZ stays unreachable this long.                                                                                |
| `auto_power_on_startup`   | bool | `true`  | Power the adapter on at boot.                                                                                                                    |
| `register_agent`          | bool | `true`  | Register Nexus as BlueZ's Agent. Disable if another process (e.g., `bluetoothctl`, `gnome-bluetooth`) owns that role.                            |

---

## `[gnss]`

GNSS fixes consumed from gpsd. See
[`dd-005-gnss-backend.md`](../dd-005-gnss-backend.md) and
[ADR-005](../nexus-architecture.md#43-key-architectural-decisions).

| Field                    | Type   | Default              | Notes                                                                                |
|--------------------------|--------|----------------------|--------------------------------------------------------------------------------------|
| `enabled`                | bool   | `false`              | Off by default — most devices don't have a GNSS receiver.                             |
| `mock`                   | bool   | `false`              | Use the in-process mock gpsd client.                                                  |
| `gpsd_endpoint`          | string | `"127.0.0.1:2947"`   | `host:port`. Standard gpsd listens on `2947`.                                         |
| `acquisition_timeout_s`  | int    | `300`                | Time in `Acquiring` before transitioning to `Degraded` (DD-005 §6.3).                 |
| `tpv_stall_timeout_s`    | int    | `30`                 | TPV silence before `Tracking → Degraded`.                                             |
| `gpsd_outage_notify_s`   | int    | `60`                 | Emit `OperatorNotification` if gpsd stays unreachable this long.                      |

---

## Putting it together

A production install on a gateway device looks like:

```toml
bus_capacity = 512
log_level = "info"

[supervision]
restart = true

[interface_monitor]
enabled = true

[profile_store]
root = "/var/lib/nexus"
key_source = "file"

[dbus]
enabled = true
bus_name = "fi.nexus1"
use_session_bus = false
allow_all_authz = false

[ethernet]
enabled = true
auth_backend = "wpa_supplicant"

[wifi]
enabled = true
backend = "wpa_supplicant"
roam_mode = "supplicant"

[bluetooth]
enabled = false

[gnss]
enabled = false
```

A battery-powered device might flip `[wifi].roam_mode` to
`"nexus"` (for fleet-managed roam decisions) and keep Bluetooth
on for companion-app connectivity.

---

## Changing config safely

1. Edit `/etc/nexus/nexus.toml`.
2. Validate the file parses before restarting:
   `nexusd --config /etc/nexus/nexus.toml` — if invalid, the
   daemon logs a context line and exits non-zero within a second.
3. `sudo systemctl restart nexus`.
4. `systemctl status nexus` must report `active (running)`;
   `journalctl -u nexus -n 50` should show one
   `"subsystem starting"` line per enabled subsystem followed by
   `"nexusd up — awaiting shutdown signal"`.
