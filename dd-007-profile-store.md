# DD-007: Profile Store — Detailed Design

**Parent:** [Nexus Architecture](./nexus-architecture.md)
**Referenced by:** [DD-002: Ethernet Backend](./dd-002-ethernet-backend.md), [DD-003: Wi-Fi Backend](./dd-003-wifi-backend.md), [DD-006: D-Bus API](./dd-006-dbus-api.md)
**Status:** Draft
**Scope:** Design of the Profile Store — persistent storage of per-interface and per-network connection profiles, including encryption at rest for credential material, atomic updates, and schema migration.

---

## Table of Contents

1. [Context](#1-context)
   - 1.1 [Repo Layout](#11-repo-layout)
2. [Responsibilities](#2-responsibilities)
3. [On-Disk Layout](#3-on-disk-layout)
   - 3.1 [Directory Structure](#31-directory-structure)
   - 3.2 [File Naming](#32-file-naming)
   - 3.3 [File Format (TOML)](#33-file-format-toml)
   - 3.4 [File Permissions](#34-file-permissions)
4. [Credential Encryption](#4-credential-encryption)
   - 4.1 [Threat Model](#41-threat-model)
   - 4.2 [Master Key Sources](#42-master-key-sources)
   - 4.3 [Encryption Scheme](#43-encryption-scheme)
   - 4.4 [Wire Format](#44-wire-format)
   - 4.5 [Key Rotation](#45-key-rotation)
5. [Rust API](#5-rust-api)
   - 5.1 [ProfileStore Trait](#51-profilestore-trait)
   - 5.2 [Profile Types](#52-profile-types)
   - 5.3 [Secret Handling](#53-secret-handling)
6. [Atomic Updates](#6-atomic-updates)
7. [Concurrent Access](#7-concurrent-access)
   - 7.1 [Per-File Advisory Locking](#71-per-file-advisory-locking)
   - 7.2 [Process-Wide Rotation Lock](#72-process-wide-rotation-lock)
   - 7.3 [Change Notification](#73-change-notification)
8. [Schema Versioning and Migration](#8-schema-versioning-and-migration)
9. [Backup and Restore](#9-backup-and-restore)
10. [Error Handling](#10-error-handling)
    - 10.1 [Corrupt Profile Files](#101-corrupt-profile-files)
    - 10.2 [Missing Master Key](#102-missing-master-key)
    - 10.3 [Decryption Failure](#103-decryption-failure)
    - 10.4 [Observability](#104-observability)
11. [Testing Strategy](#11-testing-strategy)
12. [Implementation Phases](#12-implementation-phases)

---

## 1. Context

Every technology backend in Nexus needs persistent per-connection configuration: which Ethernet interfaces authenticate with 802.1X, which Wi-Fi networks to auto-connect to, which Bluetooth devices have been paired. Some of this data is sensitive — PSKs, passphrases, EAP passwords, private-key passphrases, long-term Bluetooth link keys. Writing these to disk in cleartext is unacceptable for any non-toy deployment.

The Profile Store is the single component responsible for:

- Serializing and deserializing profiles to/from disk
- Encrypting credential material at rest
- Providing atomic update semantics so a crash mid-write doesn't corrupt a profile
- Handling schema migrations when the on-disk format evolves
- Redacting sensitive fields in diagnostic output

Backends (DD-002, DD-003, future DD-004) do not read or write files directly. They go through a `ProfileStore` trait. This centralization means encryption, locking, and schema concerns live in one place and can be tested in isolation.

### 1.1 Repo Layout

The code for this component lives at:

```
crates/
  nexus-profiles/               <- Profile Store implementation
    Cargo.toml
    src/
      lib.rs                    <- ProfileStore trait, entry point
      store.rs                  <- FilesystemProfileStore
      crypto/                   <- encryption primitives
        mod.rs                  <- Cipher trait
        chacha20poly1305.rs     <- AEAD implementation
        master_key.rs           <- MasterKeySource trait
        tpm.rs                  <- TPM-sealed master key (feature-gated)
        keyring.rs              <- keyctl-backed master key
        file.rs                 <- file-based master key (last resort)
      schema/                   <- versioned TOML schemas
        mod.rs                  <- Version enum, Migrator
        v1.rs                   <- current schema
      profile.rs                <- EthernetProfile, WifiProfile (re-exports)
      secret.rs                 <- SecretString wrapper, redaction helpers
      atomic.rs                 <- write-temp + fsync + rename helpers
      lock.rs                   <- file locking (flock)
      errors.rs
    tests/
      roundtrip.rs              <- encrypt/decrypt/serialize roundtrip
      migration.rs              <- schema v1 -> v2 migration tests
      concurrent.rs             <- multiple writers, file lock behavior
```

The `secret` module hosts a project-local `SecretString` (wrapping `secrecy::SecretString` with Nexus-specific extensions for serde tagging and redaction). This type is re-exported by `nexus-profiles`, `nexus-auth-eap`, and the technology backends so credentials never leak into debug output.

**Key dependencies:**

| Crate | Purpose | Feature gate |
|---|---|---|
| `chacha20poly1305` | AEAD cipher (§4.3) | always |
| `hkdf` | Key derivation (§4.3) | always |
| `sha2` | SHA-256 for HKDF | always |
| `blake3` | Filename hash for Wi-Fi profiles (§3.2) | always |
| `secrecy` | `SecretString` foundation | always |
| `zeroize` | Memory wipe on drop | always |
| `rand_core` + `getrandom` | OS RNG for nonces and master keys | always |
| `ulid` | Profile IDs (§3.3) | always |
| `toml` | File format | always |
| `serde` + `serde-with` | Deserialization | always |
| `chrono` | Metadata timestamps | always |
| `notify` or `inotify` | Out-of-band edit detection (§7.3) | always |
| `tokio` | Async runtime | always |
| `linux-keyutils` | Kernel keyring master key (§4.2) | `profiles-keyring` |
| `tss-esapi` | TPM 2.0 master key (§4.2) | `profiles-tpm` |
| `libc` | `flock`, `fsync`, `openat` syscalls | always |

No dependency on OpenSSL, dbus, or platform-specific crypto libraries beyond TPM (which is optional). This keeps the crate buildable for any Linux target with minimal cross-compilation friction.

---

## 2. Responsibilities

The Profile Store is responsible for:

1. **Loading** all profiles of a given technology at Nexus startup.
2. **Storing** a new or updated profile atomically — either the update is fully persisted and decryptable, or nothing changes.
3. **Deleting** profiles on operator request.
4. **Decrypting** credential material on demand at load time; holding plaintext only in memory inside `SecretString` wrappers.
5. **Encrypting** credential material when writing; never writing cleartext credentials to disk.
6. **Migrating** profile files forward when the schema version changes between releases.
7. **Redacting** credentials from any diagnostic output (logs, D-Bus property reads, error messages).
8. **Validating** profiles on load (e.g., rejecting profiles with inconsistent security fields) rather than surfacing the error later during connection.

The Profile Store is explicitly **not** responsible for:

- Deciding *when* to connect — that is the technology backend's job. The store is a data access layer.
- Hardware-specific secret storage beyond what the master-key sources described in §4.2 provide.
- Profile sync across devices — that is a fleet-management concern handled by external tooling.

---

## 3. On-Disk Layout

### 3.1 Directory Structure

```
/var/lib/nexus/
  profiles/
    version                     <- schema version marker (see §8)
    .rotation.lock              <- exclusive lock held during key rotation (§7.2)
    ethernet/
      eth0.toml
      eth1.toml
      ...
    wifi/
      <ssid_hash>.toml
      <ssid_hash>.toml
      ...
    gnss/                       <- per-device settings (see DD-005 §9)
      <ulid>.toml
    bluetooth/                  <- per-bond (see DD-004 §8.3)
      <ulid>.toml
  keys/                         <- master key material (§4.2)
    master.key                  <- only for file-based source
    master.key.old              <- retained briefly during rotation (§4.5)
    master.key.sealed           <- only for TPM-sealed source
```

Path conventions:

- **`/var/lib/nexus/`** is configurable via `[profile_store] root = "/var/lib/nexus"` in `nexus.toml` but defaults to `/var/lib/nexus/`.
- Directories are mode `0700` and owned by the `nexus` user. Files within are mode `0600`.
- The `version` marker contains a single integer per line: the current schema version. Presence of this file is how the store detects first-run vs upgrade.

### 3.2 File Naming

- **Ethernet profiles** are named by interface name: `eth0.toml`, `eth1.toml`. Interface names are stable on embedded hardware (either kernel-assigned like `eth0` or persistent-name like `enp2s0`). Profiles are loaded by enumerating the `ethernet/` directory.
- **Wi-Fi profiles** are named by a stable hash of the SSID: `<blake3-16hex>.toml`. Hashing, not encoding, because SSIDs may contain slashes, nulls, non-UTF-8 bytes, or other filesystem-hostile characters. The SSID is stored inside the file so loading works without a reverse lookup.
- **GNSS profiles** are named by ULID: `<ulid>.toml`, flat under `gnss/`. The device path (`/dev/ttyUSB0`, etc.) is stored inside the file. ULID rather than device-path because USB enumeration isn't stable across replug (see DD-005 §9.2).
- **Bluetooth bonds** are named by ULID: `<ulid>.toml`, flat under `bluetooth/` with no per-adapter nesting. The adapter address and peer device address are stored inside the file. This matches the D-Bus object path scheme in DD-006 §4 (`/fi/nexus1/profile/bluetooth/<ulid>`) and allows a single device to be bonded with multiple adapters without filename collision. Profile contents are defined in DD-004 §8.3.

Rationale for hashing Wi-Fi filenames rather than escaping: escaped filenames (`%2F` etc.) leak SSIDs in directory listings, and non-UTF-8 SSIDs become awkward to round-trip. A hash is opaque, short, always safe, and collision-free in practice (blake3-128 over realistic SSID sets has essentially zero collision probability).

### 3.3 File Format (TOML)

All profile files are TOML. Shared top-level structure:

```toml
# Schema version for this file; independent of the directory-level version
# marker (which governs overall store version). Allows per-file upgrades
# if a partial migration is ever needed.
schema_version = 1

# Opaque profile ID, stable for the life of the profile. Used in logs
# and the D-Bus API. Not the same as the filename.
#
# Generation: the caller of put_* supplies the ULID. In practice, the
# D-Bus layer's AddWifiProfile / AddEthernetProfile handlers generate
# a fresh ULID via `ulid::Ulid::new()` on first creation, and preserve
# it across subsequent Update calls. The Profile Store itself never
# generates ULIDs — this keeps the store a pure data-access layer.
id = "01HPQXR2N0Z8K9M7V3Y2F4T5W6"   # ULID

[metadata]
created_at = "2026-03-12T14:22:11Z"
updated_at = "2026-03-12T14:22:11Z"
# Optional human-friendly label shown in UIs
label = "Office Network"
```

Technology-specific sections follow. The concrete shapes come from DD-002 §8.2 for Ethernet and DD-003 §11.2 for Wi-Fi. The Profile Store's schema is the **union of all backend profile shapes**, not an attempt to genericize across technologies.

Example Wi-Fi profile (SSID hash filename, contents unencrypted wrapper with encrypted credential fields):

```toml
schema_version = 1
id = "01HPQY8S2N0Z8K9M7V3Y2F4T5W6"

[metadata]
created_at = "2026-03-12T14:22:11Z"
updated_at = "2026-03-12T14:22:11Z"
label = "Office Network"

[network]
ssid = "corp-wifi-2026"
hidden = false
priority = 10
auto_connect = true
fast_transition = true

[network.security]
type = "wpa2_enterprise"

[network.security.eap]
eap = "PEAP"
identity = "user@corp.example.com"
anonymous_identity = "anonymous@corp.example.com"
ca_cert = "/etc/nexus/certs/corp-ca.pem"
phase2 = "auth=MSCHAPV2"
domain_suffix_match = "corp.example.com"

# Encrypted fields are tagged and base64url-encoded (see §4.4)
password = { enc = "v1", nonce = "hF...ZA", ct = "Rk7...Aw" }
```

Metadata fields (`created_at`, `updated_at`, `label`) are never encrypted. Only fields that carry credential material are encrypted. The distinction is made at the Rust struct level by wrapping sensitive fields in `SecretString` (see §5.3).

### 3.4 File Permissions

- Directory `/var/lib/nexus/profiles/`: `0700`, owned by `nexus:nexus`.
- Profile files: `0600`, owned by `nexus:nexus`.
- Master key file (if file-based): `0400`, owned by `nexus:nexus`.
- The `nexus` daemon runs as `nexus:nexus`. Other users including `root` are expected NOT to read these files directly; root can always escalate, but the permissions make casual reads impossible.

On write, the temp file is created with `O_CREAT | O_EXCL | O_CLOEXEC` and mode `0600` before any data is written. Rename preserves the mode.

---

## 4. Credential Encryption

### 4.1 Threat Model

**In scope:**

- **Cold-disk attack.** An attacker obtains a storage medium (SD card, eMMC image) from a powered-off device. Without the master key, they should not be able to read credentials.
- **Casual filesystem inspection.** A user with shell access but not root shouldn't be able to read credentials. Filesystem permissions handle the common case; encryption defends against misconfigurations.
- **Profile file exfiltration over network.** Backup copies of profiles shouldn't carry cleartext credentials.

**Out of scope (defense by a different mechanism):**

- **Root compromise of a running device.** If the attacker is root on a running system, they can read process memory, access `/dev/mem`, coerce Nexus to decrypt. Encryption at rest doesn't prevent this; system hardening (IMA, dm-verity, SELinux) does.
- **Hardware-level side channels.** Cold-boot attacks on RAM, bus sniffing, etc. TPM-sealed keys help; the rest is beyond Nexus.
- **Forward secrecy across key rotation.** Profiles re-encrypted on rotation; old ciphertexts that may have been exfiltrated remain decryptable with the old key if it's recovered. Key rotation mitigates but does not eliminate this.

### 4.2 Master Key Sources

Nexus supports three master-key sources, in order of preference:

**TPM-sealed key (preferred on devices with a TPM 2.0).**
Master key is generated on first run, sealed to a specific set of PCR values for measured-boot integrity, and stored encrypted in `keys/master.key.sealed`. Unsealing requires the same PCR state, meaning kernel/bootloader tampering breaks key access — which is the right behavior for embedded deployments with secure boot. Behind the `profiles-tpm` Cargo feature.

**PCR policy.** By default Nexus seals against PCRs 0, 2, 4, 7, 11:

- **PCR 0** — firmware code (UEFI/coreboot/etc.). Breaks on firmware upgrade.
- **PCR 2** — option ROMs. Breaks if boot-time option ROMs change.
- **PCR 4** — bootloader code (GRUB, systemd-boot, U-Boot). Breaks on bootloader upgrade.
- **PCR 7** — secure boot policy state. Breaks if secure boot keys or policy change.
- **PCR 11** — `systemd-pcrphase` measurements if systemd is used. Ties unseal to a specific point in the boot sequence.

These are configurable via `[profile_store.tpm] pcrs = [0, 2, 4, 7, 11]` in `nexus.toml`. The defaults reflect a conservative "bind to the entire firmware-and-bootloader chain" posture suitable for measured-boot embedded deployments. Deployments that need to survive firmware upgrades without manual re-sealing may drop PCR 0; those that don't use systemd may drop PCR 11.

**Upgrade handling.** When a firmware/bootloader/kernel upgrade changes a sealed PCR, the master key becomes unsealable on next boot. The operator must re-seal with a fresh key using `nexusctl profiles reseal` before the new firmware boots, or accept that the Profile Store enters degraded mode (§10.2) and re-enter all credentials. For fleet deployments, the recommended pattern is: the firmware updater calls `nexusctl profiles reseal --pending <new-pcrs>` before rebooting into the new firmware, and the seal is committed on first successful boot.

**Caveat on fleet coordination.** This reseal-before-upgrade flow depends on the firmware updater knowing about Nexus — out-of-band updates (manual dd-to-eMMC, third-party OTA tooling that bypasses the Nexus hooks) will break the TPM seal. For fleet OTA designs the Nexus reseal hook needs explicit integration: a systemd-generator-based hook, an RPM/dpkg trigger, or a dedicated update-coordinator daemon. This is a production-deployment concern, not a design deficiency — TPM sealing genuinely requires the boot chain to be measured, and any upgrade path that measures must coordinate. Deployments that can't guarantee this coordination should use the keyring or file master-key sources instead, accepting the weaker hardware binding.

**Linux keyring (kernel `keyctl`).**
Master key is generated on first run and stored in the kernel keyring under a persistent user keyring or a session keyring mapped to persistent storage. The key lives in kernel memory; userspace reads it via `keyctl_read`. Requires the `keyctl` syscall (glibc 2.26+, most embedded toolchains).

In practice the keyring holds a *wrapping key*, while the real master key is stored wrapped on disk. On boot, Nexus re-derives the wrapping key and unwraps the master key into memory. For unattended embedded deployments, the wrapping key is derived from a hardware-bound identifier — typical sources in order of preference:

- **SoC-burned serial** readable via a vendor-specific interface (e.g., Rockchip `/sys/bus/nvmem/devices/rockchip-otp0/nvmem`, i.MX `fsl,imx-ocotp`, a TEE-provided device identity).
- **eFuse** mapped through nvmem — manufacturer-programmed unique ID.
- **Machine ID** from `/etc/machine-id` — software-generated but persists across reboots; weakest of these options because it's readable by any process.

The chosen source is hashed with a fixed Nexus-specific constant to derive the wrapping key. Attended deployments (e.g., a desktop Linux system) may instead prompt for a passphrase at boot, but this is out of scope for the embedded targets Nexus prioritizes. Deployments that need passphrase-based unlock should script their own wrapper around `nexusd` startup.

**File-based key (last resort).**
Master key is stored in `/var/lib/nexus/keys/master.key`, mode `0400`, owned by `nexus`. No cryptographic protection beyond filesystem permissions. Used only when neither TPM nor keyring is available. A warning is logged at every Nexus startup when this source is active:

```
warn profile_store: master key source is file-based (no hardware binding).
     Credentials at rest are protected only by filesystem permissions.
     Consider enabling TPM or keyring backends for production deployments.
```

Source is selected by config:

```toml
[profile_store]
master_key_source = "auto"   # "auto" | "tpm" | "keyring" | "file"
```

`auto` tries TPM → keyring → file in order and uses the first that succeeds.

Cargo features gate which sources are compiled in:

```toml
[features]
default = ["profiles-keyring", "profiles-file"]
profiles-tpm = ["tss-esapi"]       # TPM 2.0 via tss-esapi crate
profiles-keyring = ["linux-keyutils"]
profiles-file = []                  # always safe; no dependencies
```

The file-based source has no external dependencies and is always buildable; it remains in `default` so there's always a fallback even on minimal builds. The TPM feature pulls in a substantial dependency (`tss-esapi`) and is opt-in. Embedded integrators typically build with `--features "profiles-tpm profiles-keyring profiles-file"` and let `auto` pick.

### 4.3 Encryption Scheme

Per-field encryption using **ChaCha20-Poly1305** AEAD from the `chacha20poly1305` crate. Chosen over AES-GCM because:

- Pure Rust implementation with good performance on ARM (no AES-NI requirement).
- Smaller code size than OpenSSL.
- Well-studied, widely deployed (TLS 1.3, WireGuard, SSH).

Key schedule:

1. **Master key** (256 bits) from the chosen source (§4.2).
2. **Per-file file key** derived via HKDF-SHA256 with:
   - `ikm` = master_key
   - `salt` = fixed non-secret constant: `"nexus-profile-store-v1"` as bytes
   - `info` = `file_id` (the profile's ULID, as 16 bytes)
   - `okm` length = 32 bytes

   `info` is the per-context binding parameter in HKDF; using the file_id here means every profile file has a distinct derived key. Compromising one file key does not compromise others, and re-encrypting on save rotates the per-file key if the master key has changed.

3. **Per-field nonce**: 96 bits, generated with `rand_core::OsRng` at encrypt time. At the expected write rate (at most a few writes per minute across all profiles) the nonce space lasts effectively forever before birthday-bound collision becomes plausible — a single file would need ~2^48 encryptions before collision probability reaches 2^-32, many orders of magnitude beyond realistic use.

Associated data (AAD) for each field's encryption is:

```
AAD = file_id (16 bytes) || field_path (variable-length UTF-8)
```

where `field_path` is a dotted path like `"network.security.eap.password"`. This binds each ciphertext to its location within the file — reshuffling or cross-file copy of a ciphertext won't decrypt.

### 4.4 Wire Format

Encrypted fields are TOML inline tables with three keys:

```toml
password = { enc = "v1", nonce = "base64url-12bytes", ct = "base64url-ciphertext-plus-tag" }
```

- `enc` — version tag identifying the encryption scheme. `"v1"` is ChaCha20-Poly1305 as defined in §4.3. Future versions may change the cipher or key schedule; the tag identifies which code path to use for decryption.
- `nonce` — 12 bytes, base64url-encoded (unpadded).
- `ct` — ciphertext + 16-byte Poly1305 tag, base64url-encoded (unpadded).

Serde handles this via a custom `Deserialize` / `Serialize` implementation on `SecretString`. When deserializing:
- If the field is a plain string, treat it as cleartext (only acceptable during migration or in test fixtures; production profiles never hit this path).
- If the field is a table with `enc`/`nonce`/`ct` keys, decrypt and wrap in `SecretString`.

When serializing, always write the table form. There is no supported path to write cleartext credentials.

### 4.5 Key Rotation

Rotation happens on operator command via the D-Bus API ([DD-006 §5.2 `RotateMasterKey`](./dd-006-dbus-api.md#52-methods)) or via a Nexus CLI tool. The procedure:

1. Generate a new master key from the chosen source.
2. Install the new key as `active` while retaining the previous as `previous` — the Profile Store holds both simultaneously during rotation:

   ```rust
   struct MasterKeyRing {
       active: MasterKey,
       previous: Option<MasterKey>,
   }
   ```

3. For each profile file in the store:
   - Derive the old per-file key from `previous` and the file's `id`.
   - Decrypt every encrypted field with the old per-file key.
   - Derive the new per-file key from `active` and the file's `id`.
   - Encrypt every field with a fresh nonce under the new per-file key.
   - Write atomically (§6).

4. When every file has been rewritten, zeroize `previous` and remove the on-disk copy (see crash recovery below).

The rotation runs under the exclusive rotation lock (§7.2); no writes proceed during rotation. Expected duration is well under a second for typical profile counts (<100).

**Interaction with in-flight D-Bus calls.** Other `put_*` / `remove_*` callers block on `flock(LOCK_EX)` waiting for the rotation lock. For the expected sub-second rotation, this is invisible. For worst-case rotations (1000+ profiles at ~10 ms each), callers could block for 10+ seconds — approaching the default D-Bus method timeout (25 s for zbus, 25 s for glib-dbus). To avoid spurious client-side `Timeout` errors, the Profile Store's `put_*` / `remove_*` methods observe a 5 s internal deadline: if the rotation lock isn't acquired within 5 s, the method returns `fi.nexus.Error.ResourceBusy` with a `retry_after_ms` hint derived from the rotation job's estimated remaining time. Clients retry after the hint and see the call succeed once rotation completes.

Rotation itself never observes this deadline — it holds the lock for its full duration. The D-Bus `RotateMasterKey` method returns immediately with a job ID, so the rotation-to-completion wait doesn't count against the D-Bus call timeout anyway.

**Crash recovery.** The old master key is kept in `keys/master.key.old` throughout rotation. If the rotation process crashes mid-way, the next startup reads:
- `keys/master.key` (or `master.key.sealed` / keyring-wrapped variant) as `active`.
- `keys/master.key.old` as `previous` if present.

When decrypting a profile, the store first tries the file's per-file key derived from `active`. On decryption failure (Poly1305 tag mismatch), it falls back to the `previous`-derived key. A fallback-decrypted profile is immediately re-encrypted under `active` and rewritten atomically — this resumes the rotation incrementally on access. When every remaining profile has been migrated, the rotation completion path (triggered by the `MasterKeyRotated` signal emitter or by an explicit CLI command) zeroizes and deletes `master.key.old`.

**Source-change rotations.** Changing `master_key_source` (e.g. upgrading from file-based to TPM-sealed) is a distinct operation from in-source rotation:

1. Decrypt all profiles under the current source's key into memory.
2. Generate a new master key under the target source and make it active.
3. Re-encrypt and write every profile under the new key.
4. Remove the old source's key material.

This flow requires brief plaintext credential material in memory (inside `SecretString` wrappers per §5.3). It is not supported via an in-process D-Bus method at present — operators perform it via a dedicated `nexusctl profiles change-key-source <new>` command that takes an exclusive rotation lock and runs to completion before Nexus accepts normal traffic. If the process crashes mid-conversion, recovery uses the same `active`/`previous` fallback described above, but the sources for the two keys are different — the code path handles this by holding each key's source descriptor alongside the key itself.

---

## 5. Rust API

### 5.1 ProfileStore Trait

```rust
#[async_trait]
pub trait ProfileStore: Send + Sync {
    /// Load all Ethernet profiles from disk.
    /// Returns profiles sorted by ULID ascending, which requires reading
    /// every file's `id` field before sorting — a minor cost for typical
    /// profile counts (2–5 interfaces) and amortized across the life of the
    /// daemon since load_ethernet is called rarely (startup + inotify-triggered
    /// reloads). Filename order on disk is ifname order, not ULID order.
    /// Ordering is stable across loads but has no semantic meaning — profile
    /// selection priority comes from per-profile fields, not load order
    /// (see DD-002 §3, DD-003 §6.1). Malformed individual profiles are
    /// logged and skipped; the load proceeds for the remainder.
    async fn load_ethernet(&self) -> Result<Vec<EthernetProfile>>;

    /// Load a single Ethernet profile by interface name.
    /// Returns None if no profile exists for that interface.
    async fn load_ethernet_profile(&self, ifname: &str) -> Result<Option<EthernetProfile>>;

    /// Load all Wi-Fi profiles.
    /// Same ordering contract as load_ethernet: ULID-ascending, stable,
    /// semantically meaningless.
    async fn load_wifi(&self) -> Result<Vec<WifiProfile>>;

    /// Store or overwrite a Wi-Fi profile. The SSID hash derived from
    /// profile.network.ssid determines the on-disk filename; profiles
    /// with the same SSID replace each other.
    ///
    /// The profile.id (ULID) is preserved when overwriting an existing
    /// file ONLY IF the caller passes the existing id. Passing a fresh
    /// ULID to overwrite an existing profile results in the on-disk ULID
    /// changing — which is usually a bug, because the D-Bus object path
    /// (which encodes the ULID) would shift. The D-Bus layer's AddWifiProfile
    /// is expected to return AlreadyExists before reaching put_wifi for
    /// same-SSID collisions, and Update is expected to reuse the existing
    /// id. Direct callers of put_wifi (migration tooling, tests) should
    /// understand this contract.
    async fn put_wifi(&self, profile: &WifiProfile) -> Result<()>;

    /// Remove a Wi-Fi profile by SSID hash.
    async fn remove_wifi(&self, ssid_hash: &str) -> Result<()>;

    /// Store or overwrite an Ethernet profile. The profile.interface.name
    /// determines the on-disk filename; same ULID-stability notes apply
    /// as for put_wifi.
    async fn put_ethernet(&self, profile: &EthernetProfile) -> Result<()>;

    /// Remove an Ethernet profile by interface name.
    async fn remove_ethernet(&self, ifname: &str) -> Result<()>;

    // ---- GNSS (see DD-005 §9) ------------------------------------------

    /// Load all GNSS device profiles. Ordering contract matches
    /// load_ethernet / load_wifi.
    async fn load_gnss(&self) -> Result<Vec<GnssDeviceProfile>>;

    /// Load a GNSS profile by kernel device path (e.g., "/dev/ttyUSB0").
    /// Linear scan over stored profiles; counts are small enough in
    /// practice (one or two GNSS devices per system) that no index is
    /// needed. Note that device-path matching is fragile across
    /// USB-device replug — see DD-005 §9.2.
    async fn load_gnss_profile_by_path(&self, device_path: &str)
        -> Result<Option<GnssDeviceProfile>>;

    /// Store or overwrite a GNSS profile. ULID-keyed on disk; the
    /// ULID is preserved on overwrite when the caller passes the
    /// existing id.
    async fn put_gnss(&self, profile: &GnssDeviceProfile) -> Result<()>;

    /// Remove a GNSS profile by ULID.
    async fn remove_gnss(&self, id: &Ulid) -> Result<()>;

    // ---- Bluetooth (see DD-004 §8.3) -----------------------------------

    /// Load all Bluetooth profiles. Ordering contract matches the
    /// other load_* methods.
    async fn load_bluetooth(&self) -> Result<Vec<BluetoothProfile>>;

    /// Load a Bluetooth profile by device Bluetooth address. Linear
    /// scan; counts are small in practice (typically 20-30 bonded
    /// devices, worst case a few hundred) so no index is needed.
    async fn load_bluetooth_profile_by_address(&self, address: &MacAddr)
        -> Result<Option<BluetoothProfile>>;

    /// Store or overwrite a Bluetooth profile. ULID-keyed on disk.
    /// The Bluetooth address is inside the profile; address-collision
    /// is not possible for bonded devices (BlueZ enforces one bond
    /// per (adapter, peer) pair) but the store does not enforce this
    /// — last write wins.
    async fn put_bluetooth(&self, profile: &BluetoothProfile) -> Result<()>;

    /// Remove a Bluetooth profile by ULID.
    async fn remove_bluetooth(&self, id: &Ulid) -> Result<()>;

    // ---- Cross-kind operations -----------------------------------------

    /// Mark a profile as having invalid credentials. This flag is
    /// consumed by the backend's retry logic (see DD-003 §12.3).
    /// Idempotent. The ProfileRef enum statically distinguishes the
    /// key format per profile kind.
    async fn set_credentials_invalid(
        &self,
        reference: ProfileRef<'_>,
        invalid: bool,
    ) -> Result<()>;

    /// Rotate the master key (§4.5). Expensive; call sparingly.
    async fn rotate_master_key(&self) -> Result<RotateReport>;
}

pub enum ProfileKind {
    Ethernet,
    Wifi,
    Gnss,
    Bluetooth,
}

pub enum ProfileRef<'a> {
    /// Ethernet profile identified by its interface name.
    Ethernet { ifname: &'a str },
    /// Wi-Fi profile identified by the SSID hash used as its filename.
    Wifi { ssid_hash: &'a str },
    /// GNSS profile identified by its ULID. GNSS profiles are ULID-keyed
    /// on disk because device paths aren't stable across USB replug.
    Gnss { id: &'a Ulid },
    /// Bluetooth profile identified by its ULID. Like GNSS, Bluetooth
    /// profiles are ULID-keyed rather than address-keyed, for D-Bus
    /// path stability.
    Bluetooth { id: &'a Ulid },
}

pub struct RotateReport {
    pub profiles_rewritten: u32,
    pub duration: Duration,
}
```

The concrete implementation is `FilesystemProfileStore` in `crates/nexus-profiles/src/store.rs`. A `MemoryProfileStore` (for tests) lives alongside.

### 5.2 Profile Types

```rust
/// Ethernet profile as stored. Corresponds to one TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EthernetProfile {
    pub id: Ulid,
    pub schema_version: u32,
    #[serde(default)]
    pub metadata: ProfileMetadata,
    pub interface: EthInterfaceSettings,
    pub dot1x: Option<Dot1xSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EthInterfaceSettings {
    pub name: String,
    pub auto_connect: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dot1xSettings {
    pub enabled: bool,
    pub eap: Dot1xEapConfig,  // from nexus-auth-eap
}

/// Wi-Fi profile as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiProfile {
    pub id: Ulid,
    pub schema_version: u32,
    #[serde(default)]
    pub metadata: ProfileMetadata,
    pub network: WifiNetworkSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiNetworkSettings {
    pub ssid: Ssid,
    #[serde(default)]
    pub hidden: bool,
    pub priority: i32,
    pub auto_connect: bool,
    #[serde(default)]
    pub fast_transition: bool,
    pub security: SecurityConfig,  // from nexus-wifi
    #[serde(default)]
    pub bssid_preferred: Option<MacAddr>,
    #[serde(default)]
    pub bssid_blacklist: Vec<MacAddr>,
    #[serde(default)]
    pub scan_freqs: Vec<u32>,
    #[serde(default)]
    pub credentials_invalid: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileMetadata {
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub label: Option<String>,
}
```

`WifiProfile` here is the serializable form used by the Profile Store. The Wi-Fi backend's in-memory `WifiProfile` from DD-003 §6.1 is the same struct — the store returns it directly, and the backend calls `to_network_config()` to derive the supplicant-facing type.

**GNSS and Bluetooth profiles** follow the same pattern: the canonical struct definition lives with the backend that owns the profile (`GnssDeviceProfile` in DD-005 §9, `BluetoothProfile` in DD-004 §8.3), and the store holds the serializable form. Neither currently has credential fields — GNSS profiles hold device-specific tuning (rate caps, accuracy filters); Bluetooth profiles hold preferences and a bond-adapter tag but never link keys (those stay in BlueZ's own on-disk store at `/var/lib/bluetooth/`). So neither needs the dual-struct pattern described in §5.3 — they can use a single struct for both in-memory and on-disk form. Encryption at the Profile Store level still applies to the whole file for consistency with Wi-Fi/Ethernet, but nothing inside the file is individually secret.

### 5.3 Secret Handling

Credential fields are typed as `SecretString` (a wrapper around `secrecy::SecretString` with Nexus-specific redaction helpers):

```rust
/// String credential that never appears in Debug output and is
/// zeroized on drop.
#[derive(Clone)]
pub struct SecretString(secrecy::SecretString);

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl SecretString {
    /// Expose the underlying string. Callers must not log or send
    /// the returned reference except through the Profile Store's
    /// encrypt-on-write path.
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}
```

**`SecretString` is not `Serialize` or `Deserialize`.** Making it directly serializable via Serde would either encrypt every serialization (wrong — we want in-memory copies to stay as `SecretString`, not as ciphertext) or silently reveal secrets on `toml::to_string`. Neither is acceptable.

Instead, the Profile Store defines a **dual-struct pattern**: each credential-bearing profile type has two shapes, one for in-memory use and one for on-disk storage.

```rust
// --- In-memory shape used by backends; not Serialize/Deserialize. ---

pub struct WifiProfile {
    pub id: Ulid,
    pub schema_version: u32,
    pub metadata: ProfileMetadata,
    pub network: WifiNetworkSettings,
}

pub struct WifiNetworkSettings {
    pub ssid: Ssid,
    pub hidden: bool,
    pub priority: i32,
    pub auto_connect: bool,
    pub fast_transition: bool,
    pub security: SecurityConfig,           // contains SecretString fields
    pub bssid_preferred: Option<MacAddr>,
    pub bssid_blacklist: Vec<MacAddr>,
    pub scan_freqs: Vec<u32>,
    pub credentials_invalid: bool,
}

// --- On-disk shape used for serialization; only holds encrypted blobs. ---

#[derive(Serialize, Deserialize)]
struct WifiProfileOnDisk {
    id: Ulid,
    schema_version: u32,
    metadata: ProfileMetadata,
    network: WifiNetworkSettingsOnDisk,
}

#[derive(Serialize, Deserialize)]
struct WifiNetworkSettingsOnDisk {
    ssid: Ssid,
    hidden: bool,
    priority: i32,
    auto_connect: bool,
    fast_transition: bool,
    security: SecurityConfigOnDisk,         // credential fields are EncryptedBlob
    bssid_preferred: Option<MacAddr>,
    bssid_blacklist: Vec<MacAddr>,
    scan_freqs: Vec<u32>,
    credentials_invalid: bool,
}

/// TOML-native representation of an encrypted field.
#[derive(Serialize, Deserialize)]
pub struct EncryptedBlob {
    pub enc: String,            // version tag, e.g. "v1"
    #[serde(with = "base64url")]
    pub nonce: [u8; 12],
    #[serde(with = "base64url")]
    pub ct: Vec<u8>,            // ciphertext + Poly1305 tag
}
```

The Profile Store's write path constructs `WifiProfileOnDisk` by walking the in-memory `WifiProfile`, calling `Cipher::encrypt(file_key, nonce, field_path, plaintext)` on every `SecretString`, and building `EncryptedBlob` inline tables. The read path does the inverse: `toml::from_str` yields `WifiProfileOnDisk`, then the store walks it and calls `Cipher::decrypt(...)` to produce the in-memory `WifiProfile` with `SecretString` fields.

Backends hold the in-memory form (`WifiProfile` with `SecretString` fields). They never serialize profiles themselves — all persistence goes through `ProfileStore::put_*`, which accepts the in-memory form and handles the on-disk conversion internally. This makes it impossible to accidentally serialize plaintext credentials: the only type that appears in a `Serialize` impl is `EncryptedBlob`, which holds ciphertext.

**Migration fallback.** During the 0.x development cycle, a profile file may still contain a bare-string credential where an `EncryptedBlob` is now expected (e.g., a hand-crafted test fixture). The on-disk representation for credential fields is actually a small enum:

```rust
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum CredentialOnDisk {
    Encrypted(EncryptedBlob),
    #[cfg(feature = "profiles-accept-plaintext")]
    Plaintext(String),   // only deserialized, never serialized
}
```

With the `profiles-accept-plaintext` feature enabled (default during 0.x), the read path accepts either variant and re-writes as `Encrypted` on next `put_*`. With the feature disabled (1.0 and later), only the `Encrypted` variant deserializes; a plaintext field triggers the quarantine path in §10.1.

---

## 6. Atomic Updates

Every `put_*` or `remove_*` call follows this sequence:

1. Acquire the appropriate advisory lock. For an existing target file, `flock(LOCK_EX)` on that file. For a not-yet-existing target file (first write), `flock(LOCK_EX)` on the parent directory — this serializes concurrent creators of the same filename. On successful creation, subsequent writes lock the file directly.
2. Serialize the profile to a `Vec<u8>`.
3. Open a temp file in the same directory with an `.nexus.tmp.<pid>.<random>` suffix, mode `0600`, `O_CREAT | O_EXCL | O_CLOEXEC`.
4. Write the full serialized content.
5. `fsync(tmp_fd)` — flush data to disk before the rename.
6. `rename(tmp, final)` — atomic on POSIX filesystems when source and destination are in the same directory.
7. `fsync(parent_dir_fd)` — flush the directory entry so the rename survives a crash.
8. Release the lock.

If any step fails, the temp file is removed on the way out. The final file is never in a partial state.

The `nexus-profiles::atomic::write_file_atomic(path, contents)` helper encapsulates this, including the new-file vs existing-file lock selection.

**Why the double fsync.** Without the parent-directory fsync, some filesystems (ext4 with `data=writeback`, some network filesystems) may commit the file data but not the directory entry. On crash + remount the file either doesn't exist or has the old name. The directory fsync forces the rename to be durable.

**Performance cost.** Two fsyncs per write is ~5–20 ms on typical eMMC or SSD, depending on filesystem and write barriers. Profile writes are infrequent (operator actions, not steady-state traffic), so this is acceptable.

---

## 7. Concurrent Access

Nexus is a single-process daemon — normal code paths have just one writer. However:

- The operator may run a Nexus CLI tool that edits profiles while the daemon is running.
- A future `nexus-config` admin tool may modify profiles out-of-band.
- Two D-Bus callers may concurrently modify different profiles.

The Profile Store protects against these cases via:

### 7.1 Per-File Advisory Locking

Every `put_*` and `remove_*` takes an exclusive `flock(LOCK_EX)` on the target file. For `remove_*` when the target exists, the lock is on the file itself; when the target doesn't exist, no lock is needed (unlink-of-nonexistent is a no-op and returns `NotFound`). `flock` on a just-opened file descriptor serializes writes among any process that follows this protocol.

Readers (`load_*`) do NOT take locks. They rely on two properties:

- **Atomic replace via `rename()`.** A write that's in progress keeps its data in a temp file until the final `rename()` — readers opening the real path either see the old content or the new content, never a partial write.
- **Open-then-read stability.** On Linux, a reader that successfully `open()`s a file retains access to its inode even if another process `unlink()`s it afterward. `read()` continues to work. A `remove_*` concurrent with a `load_*` does not cause the reader to fail mid-read.

For `remove_*` specifically: the implementation opens the file for `flock`, acquires the lock, then `unlink()`s. Any concurrent reader that opened the same path before the unlink continues reading the old content; any reader that opens after the unlink gets `NotFound`.

### 7.2 Process-Wide Rotation Lock

A single `/var/lib/nexus/profiles/.rotation.lock` file is taken exclusively during `rotate_master_key`. All other `put_*`, `remove_*`, and `rotate_master_key` operations block waiting for it. `load_*` does not acquire this lock — reads during rotation see the profile being rewritten at whichever atomic snapshot the filesystem exposes.

The filename starts with `.` so it doesn't clutter default `ls` listings; operators reading the directory with `ls -la` will see it and should understand it is held only briefly during rotation. An orphaned lock (file present but no process holds `flock`) is harmless: the next `rotate_master_key` call's `flock(LOCK_EX)` succeeds immediately.

### 7.3 Change Notification

After a successful `put_*` / `remove_*`, the Profile Store emits a `NexusEvent::ProfileChanged { kind, key }` on the event bus. Backends subscribe and reload the affected profile. This lets out-of-band edits (by CLI tools that share the store library) propagate into the running daemon without polling.

Out-of-band edits by tools that don't share the Nexus library (hand-editing the TOML, for example) won't emit `ProfileChanged`. Those edits are detected via an `inotify` watch on the profile directories, which triggers a targeted reload. Inotify is cheap and well-supported on every embedded Linux target Nexus runs on.

---

## 8. Schema Versioning and Migration

Two levels of versioning:

**Store version.** The top-level `/var/lib/nexus/profiles/version` file contains a single integer, the current store version. Store version changes when the directory layout or cross-file contracts change (adding a new technology subdirectory, changing naming conventions, etc.).

**Per-file schema version.** Each profile's `schema_version` field marks the schema used to produce that file. Schema version changes when the contents of a profile file change (adding a field, renaming a field, changing how security is encoded).

On startup, Nexus compares the store version to the current code's expected version. If they differ:

1. Refuse to write until migration completes (locks the store exclusively).
2. Run migrators in order from current-on-disk version to current-in-code version.
3. Each migrator reads every profile, upgrades to the next schema version, and writes atomically.
4. After all migrators complete, update the top-level version file.

Migrators live in `nexus-profiles/src/schema/` as individual modules — `v1_to_v2.rs`, `v2_to_v3.rs`, etc. Each exposes `async fn migrate(store: &FilesystemProfileStore) -> Result<MigrationReport>`.

**Forward compatibility.** A newer Nexus reading an older store migrates forward. An older Nexus reading a newer store refuses to start:

```
error profile_store: store version 3 is newer than this binary's supported version 2.
      Upgrade Nexus before downgrading would be possible.
```

There is no automatic downgrade path. Downgrade is handled by operator-run backup/restore (§9).

**Testing migrations.** Each migrator has a golden-fixture test: a directory of known-good profiles in the old version, with the expected post-migration output. Migrations are run and the output compared byte-for-byte. New migrators must add fixtures; CI fails if any migrator lacks test coverage.

**Plaintext-to-encrypted migration.** During the 0.x development cycle, a profile file may contain a bare-string credential where an `EncryptedBlob` is now expected (from pre-encryption builds or test fixtures). Handling is described in §5.3 under "Migration fallback": the `profiles-accept-plaintext` Cargo feature (default during 0.x, removed at 1.0) allows the read path to accept either a plaintext string or an `EncryptedBlob` for credential fields; a plaintext-accepted profile is re-encrypted on the next `put_*`. With the feature disabled, plaintext credentials quarantine the file per §10.1. This path is for upgrade-from-test-deployment scenarios; production deployments that start on encryption-capable builds never produce plaintext files.

---

## 9. Backup and Restore

Backup is a user-space concern: tar up `/var/lib/nexus/` including `keys/`. Nexus provides D-Bus methods on the Manager (see [DD-006 §5.2](./dd-006-dbus-api.md#52-methods)) to coordinate a consistent snapshot:

- `fi.nexus.Manager.FreezeForBackup()` — takes the process-wide rotation lock (§7.2), preventing writes. Returns a lease token.
- `fi.nexus.Manager.ReleaseBackupLease(token)` — releases the lock.

A backup script calls `Freeze`, tars the directory, calls `Release`. If the script crashes without releasing, the lease expires after 60 seconds.

**Restore** is straightforward: stop Nexus, replace `/var/lib/nexus/`, start Nexus. The restored store must include `keys/` or profiles will be undecryptable.

**Cross-device restore** is intentionally not supported for the TPM master-key source — the sealed key is bound to the source device's PCR state. For keyring and file sources, the key is restorable and cross-device restore works. Operators planning cross-device deployment should choose the keyring or file source and script the key distribution themselves.

---

## 10. Error Handling

### 10.1 Corrupt Profile Files

On load, if a profile file fails to deserialize or decrypt:

- Log with the file path and a classification (`toml_parse_error`, `decrypt_error`, `schema_violation`).
- Move the file to `/var/lib/nexus/profiles/.quarantine/<original>.<timestamp>` so it's preserved for forensics.
- Continue loading remaining profiles.
- Emit `NexusEvent::ProfileCorrupt { kind, key, reason }` so the D-Bus layer can surface an operator notification.

Quarantined files are never automatically deleted. Operator intervention is required to clean them up. This is intentional — silent deletion of a potentially important credential is worse than accumulating junk files.

### 10.2 Missing Master Key

If the configured master-key source fails to produce a key at startup:

- For `tpm`: PCR mismatch — typically indicates kernel or bootloader tampering, or an unsealed-key firmware upgrade. Log with loud emphasis; continue startup with the Profile Store in degraded mode (no profile decryption possible; no profiles loaded; no writes permitted). Surface via D-Bus so the operator can investigate.
- For `keyring`: key not found — likely a fresh boot without the unwrap-key setup. Same degraded mode.
- For `file`: file missing or wrong permissions — generate a new key on first run; on subsequent runs, degraded mode.

Degraded mode means the backends behave as if there are no saved profiles. The device can still connect to open networks via D-Bus operator commands; it just can't load saved credential-bearing profiles.

### 10.3 Decryption Failure

Decryption failure on a specific field within an otherwise-valid profile is treated the same as file corruption (§10.1). The whole profile is quarantined — a profile with a missing PSK is not usable, and partial decryption creates ambiguity about what to do.

### 10.4 Observability

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `nexus_profiles_loaded_total` | counter | `kind` (`ethernet`/`wifi`) | Successfully loaded profiles at startup and on reload |
| `nexus_profiles_stored` | gauge | `kind` | Current profile count |
| `nexus_profiles_writes_total` | counter | `kind`, `outcome` (`success`/`lock_contention`/`io_error`) | Write operations |
| `nexus_profiles_write_duration_seconds` | histogram | `kind` | End-to-end write duration including fsync |
| `nexus_profiles_corrupt_total` | counter | `kind`, `reason` | Profiles quarantined |
| `nexus_profiles_master_key_source` | gauge | `source` | 1 for the active source, 0 otherwise |
| `nexus_profiles_rotation_total` | counter | `outcome` | Master-key rotation attempts |
| `nexus_profiles_rotation_duration_seconds` | histogram | — | Rotation duration |
| `nexus_profiles_migration_total` | counter | `from_version`, `to_version`, `outcome` | Schema migrations |

---

## 11. Testing Strategy

### 11.1 Unit Tests

- **Encryption roundtrip.** Encrypt + decrypt for every field type under every master-key source (TPM mocked).
- **AAD binding.** Ciphertexts encrypted with one `file_id` fail to decrypt under a different `file_id`. Same for `field_path`.
- **Redaction.** `format!("{:?}", secret)` never contains the underlying bytes.
- **SecretString zeroization.** After drop, memory is wiped (verified via tooling like `valgrind` where possible, or by the `zeroize` crate's guarantees).

### 11.2 Integration Tests

- **Full store roundtrip.** Populate a store with a variety of Ethernet and Wi-Fi profiles, close it, reopen, verify all profiles survive identically.
- **Atomic write crash recovery.** Mock the filesystem to fail between rename and directory fsync; verify the old file is still readable and the new file is not partially present.
- **Concurrent writers.** Multiple processes writing different profiles simultaneously; verify all writes succeed and no corruption.
- **Out-of-band edit.** Edit a file directly; verify the inotify-triggered reload picks it up.

### 11.3 Migration Tests

- **Golden fixtures.** For each schema version transition, a fixture directory is the expected input and a fixture directory is the expected output. Migration is run and the output compared.
- **Forward-incompat refusal.** Create a store with a higher version number than the binary supports; verify Nexus refuses to start with the expected error message.

### 11.4 Fault Injection

- **Filesystem errors.** Simulate `ENOSPC`, `EIO`, permission-denied mid-write; verify graceful degradation.
- **Master-key source failures.** TPM returns PCR-mismatch error; keyring key is revoked; file is deleted — verify degraded-mode entry and operator-visible signals.
- **Corrupt decryption.** Feed malformed ciphertext; verify quarantine and no crash.

---

## 12. Implementation Phases

### Phase 1 — Types and Plaintext Roundtrip

`crates/nexus-profiles/src/profile.rs`, `schema/v1.rs`. Define the profile structs, TOML schema, `ProfileMetadata`, `ProfileKind`. Implement serde without encryption — plaintext credentials only, for bootstrapping.

**Exit criterion:** `EthernetProfile` and `WifiProfile` roundtrip through TOML with all fields preserved. Unit tests cover the v1 schema exhaustively.

### Phase 2 — Atomic Writes and Filesystem Store

`src/atomic.rs`, `src/lock.rs`, `src/store.rs`. Implement `FilesystemProfileStore` with plaintext credentials. Write-temp + fsync + rename. Advisory file locks. Inotify reload.

**Exit criterion:** Can create, read, update, delete profiles via the trait. Crash tests confirm atomicity.

### Phase 3 — Encryption Primitives

`src/crypto/mod.rs`, `src/crypto/chacha20poly1305.rs`. `Cipher` trait, ChaCha20-Poly1305 implementation, HKDF key derivation, per-field encrypt/decrypt.

**Exit criterion:** Standalone unit tests encrypt a variety of fields, including empty strings, multi-MB blobs, and UTF-8 edge cases. AAD binding verified.

### Phase 4 — Master Key Sources

`src/crypto/master_key.rs`, `src/crypto/file.rs`, `src/crypto/keyring.rs`, `src/crypto/tpm.rs`. Implement the three sources behind feature gates.

**Exit criterion:** `auto` mode correctly probes in order. Each source has integration tests that actually hit the subsystem (real keyring, real file, mock TPM). Missing-key paths produce the degraded-mode signal.

### Phase 5 — Wire Into Store

Integrate encryption into `FilesystemProfileStore`. `SecretString` serde glue requires the encryption context; enforce this at the type level so plaintext writes are impossible in release builds.

**Exit criterion:** The full integration test suite from §11.2 passes. A fresh store end-to-end run through CRUD operations produces only encrypted credential fields on disk.

### Phase 6 — Schema Versioning and Migration Framework

`src/schema/mod.rs`. Version enum, `Migrator` trait, `run_migrations()` entry point. No actual migrators yet — just the plumbing.

**Exit criterion:** Framework can be exercised with a fake migrator in tests. Forward-incompat refusal works.

### Phase 7 — Key Rotation

`ProfileStore::rotate_master_key` implementation. Per-file re-encryption under exclusive rotation lock. Old-key fallback for crash recovery.

**Exit criterion:** Rotation runs cleanly on a store of 100 profiles in under 1 s. Crash mid-rotation recovers on next startup.

### Phase 8 — Observability and Operator Integration

Metrics per §10.4. `ProfileChanged` event emission. D-Bus backup-lease methods (stubbed until DD-006 implementation).

**Exit criterion:** Metrics visible via Prometheus scrape. `ProfileChanged` events correctly triggered by every mutation path.

### Phase 9 — Hardening

Fault injection tests (§11.4). Soak tests (repeat writes for 24 hours, verify no file-handle or memory leaks). Verify zeroization via instrumentation.

**Exit criterion:** All failure modes in §10 produce the documented degraded behavior. No leaks under soak. Ready for consumers (DD-002, DD-003) to depend on.

---

## Related Documents

- [Nexus Architecture](./nexus-architecture.md) — Parent architecture document
- [DD-002: Ethernet Backend](./dd-002-ethernet-backend.md) — Primary consumer for Ethernet profiles (§8.2)
- [DD-003: Wi-Fi Backend](./dd-003-wifi-backend.md) — Primary consumer for Wi-Fi profiles (§11.2)
- [DD-006: D-Bus API](./dd-006-dbus-api.md) — Exposes profile CRUD methods to external callers
- DD-004: Bluetooth Backend *(forthcoming)* — Will add bond storage under the Bluetooth subdirectory
