# Example Nexus configuration fragments

Nexus loads a single `/etc/nexus/nexus.toml` on startup; it does
**not** merge files from `/etc/nexus/nexus.conf.d/` at runtime.
The snippets in this directory are templates — copy the sections
you need into your `nexus.toml`.

| Fragment          | Purpose                                                                 |
|-------------------|-------------------------------------------------------------------------|
| `wifi.conf`       | Enables the Wi-Fi subsystem with wpa_supplicant (the default backend).  |
| `bluetooth.conf`  | Enables the Bluetooth subsystem (talks to BlueZ over the system bus).   |
| `gnss.conf`       | Enables the GNSS subsystem (reads from a local gpsd on `127.0.0.1:2947`).|
| `dev-session.conf`| Flips the daemon onto the session bus with PolicyKit disabled; for developer VMs. |

Full field reference: [`docs/configuration.md`](../../docs/configuration.md).
