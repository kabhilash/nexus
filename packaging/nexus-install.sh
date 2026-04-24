#!/usr/bin/env bash
#
# nexus-install.sh — developer-grade installer for Nexus. Tested
# on Ubuntu 24.04; should work on any systemd distro with PolicyKit
# and D-Bus.
#
# What it does (in order):
#   1. Fails fast if not running as root.
#   2. Creates the `nexus` system user/group with no home, no shell.
#   3. Installs the `nexusd` binary to /usr/local/bin (or a caller-
#      provided --bindir), the default config to /etc/nexus/
#      nexus.toml, and the example PolicyKit rules to
#      /etc/polkit-1/rules.d/.
#   4. Drops the systemd unit, tmpfiles entry, and PolicyKit action
#      file into their standard locations.
#   5. Runs `systemd-tmpfiles --create` to materialize
#      /var/lib/nexus.
#   6. Reloads systemd + PolicyKit; enables nexus.service.
#
# It does NOT start the service — call `systemctl start nexus`
# after reviewing /etc/nexus/nexus.toml.
#
# All paths are overridable via environment variables at the top.

set -euo pipefail

# ---- Configurable paths ----------------------------------------------------

BINDIR="${BINDIR:-/usr/local/bin}"
CONFDIR="${CONFDIR:-/etc/nexus}"
UNITDIR="${UNITDIR:-/lib/systemd/system}"
TMPFILESDIR="${TMPFILESDIR:-/usr/lib/tmpfiles.d}"
POLKIT_ACTIONS="${POLKIT_ACTIONS:-/usr/share/polkit-1/actions}"
POLKIT_RULES="${POLKIT_RULES:-/etc/polkit-1/rules.d}"
# D-Bus system bus policy. `/etc/dbus-1/system.d/` is the
# administrator-owned tree; distro packages drop into
# `/usr/share/dbus-1/system.d/`. We use /etc so distro updates
# don't reach in and overwrite.
DBUS_SYSTEM_D="${DBUS_SYSTEM_D:-/etc/dbus-1/system.d}"

# Where this script lives — used to locate the packaging/ tree
# when running from a source checkout.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

NEXUS_USER="${NEXUS_USER:-nexus}"
NEXUS_GROUP="${NEXUS_GROUP:-nexus}"

# ---- Helpers ---------------------------------------------------------------

log() { printf '[nexus-install] %s\n' "$*"; }
die() { printf '[nexus-install] error: %s\n' "$*" >&2; exit 1; }

require_root() {
    if [[ "$(id -u)" -ne 0 ]]; then
        die "this script must be run as root (try: sudo $0)"
    fi
}

have() { command -v "$1" >/dev/null 2>&1; }

# ---- Arg parsing -----------------------------------------------------------

BINARY=""
DRY_RUN=0

usage() {
    cat <<USAGE
Usage: $0 [--binary PATH] [--bindir DIR] [--dry-run]

  --binary PATH    Pre-built nexusd binary. Defaults to
                   \$REPO_ROOT/target/release/nexusd, falling back
                   to \$REPO_ROOT/target/debug/nexusd.
  --bindir DIR     Install directory (default: $BINDIR).
  --dry-run        Print what would happen without making changes.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)    BINARY="$2"; shift 2 ;;
        --bindir)    BINDIR="$2"; shift 2 ;;
        --dry-run)   DRY_RUN=1; shift ;;
        -h|--help)   usage; exit 0 ;;
        *)           die "unknown argument: $1 (see --help)" ;;
    esac
done

if [[ -z "$BINARY" ]]; then
    if [[ -x "$REPO_ROOT/target/release/nexusd" ]]; then
        BINARY="$REPO_ROOT/target/release/nexusd"
    elif [[ -x "$REPO_ROOT/target/debug/nexusd" ]]; then
        BINARY="$REPO_ROOT/target/debug/nexusd"
    else
        die "no built nexusd found; run 'cargo build --release -p nexus-daemon' or pass --binary PATH"
    fi
fi
[[ -x "$BINARY" ]] || die "binary is not executable: $BINARY"

# `install -D` does mkdir + copy + chmod atomically. `maybe` wraps
# it (and other mutations) so --dry-run can print instead of act.
maybe() {
    if [[ "$DRY_RUN" -eq 1 ]]; then
        printf '  would: %s\n' "$*"
    else
        "$@"
    fi
}

# ---- Preflight -------------------------------------------------------------

require_root

for tool in install systemctl; do
    have "$tool" || die "$tool is required but not installed"
done
# PolicyKit is not strictly required, but we warn loudly if missing
# because the daemon's mutating methods return AuthFailed without it.
if ! have pkaction; then
    log "warning: PolicyKit (pkaction) not found — mutating methods will deny until you install polkit"
fi
# journalctl is handy but not required to install.

# ---- Step 1: user/group ----------------------------------------------------

if ! getent group "$NEXUS_GROUP" >/dev/null; then
    log "creating group $NEXUS_GROUP"
    maybe groupadd --system "$NEXUS_GROUP"
else
    log "group $NEXUS_GROUP already exists — keeping as-is"
fi

if ! getent passwd "$NEXUS_USER" >/dev/null; then
    log "creating user $NEXUS_USER"
    maybe useradd --system \
        --gid "$NEXUS_GROUP" \
        --home-dir /var/lib/nexus \
        --no-create-home \
        --shell /usr/sbin/nologin \
        --comment "Nexus connectivity manager" \
        "$NEXUS_USER"
else
    log "user $NEXUS_USER already exists — keeping as-is"
fi

# Operator groups referenced by the example PolicyKit rules. We
# create them empty; the operator adds members afterwards.
for g in nexus-admin nexus-user; do
    if ! getent group "$g" >/dev/null; then
        log "creating group $g (empty; add members via 'usermod -aG $g <user>')"
        maybe groupadd --system "$g"
    fi
done

# ---- Step 2: binary --------------------------------------------------------

log "installing nexusd → $BINDIR/nexusd"
maybe install -D -m 0755 "$BINARY" "$BINDIR/nexusd"

# ---- Step 3: config --------------------------------------------------------

if [[ ! -f "$CONFDIR/nexus.toml" ]]; then
    log "writing default config $CONFDIR/nexus.toml"
    maybe install -D -m 0644 \
        "$REPO_ROOT/crates/nexus-daemon/tests/fixtures/minimal.toml" \
        "$CONFDIR/nexus.toml"
else
    log "keeping existing $CONFDIR/nexus.toml"
fi

# Ship the example drop-ins so operators can copy fragments.
log "installing example drop-ins → $CONFDIR/examples/"
for f in "$REPO_ROOT"/packaging/nexus.conf.d/*.conf; do
    [[ -f "$f" ]] || continue
    maybe install -D -m 0644 "$f" "$CONFDIR/examples/$(basename "$f")"
done
maybe install -D -m 0644 \
    "$REPO_ROOT/packaging/nexus.conf.d/README.md" \
    "$CONFDIR/examples/README.md"

# ---- Step 4: systemd + tmpfiles + polkit ----------------------------------

log "installing nexus.service → $UNITDIR/nexus.service"
maybe install -D -m 0644 \
    "$REPO_ROOT/packaging/nexus.service" \
    "$UNITDIR/nexus.service"

log "installing tmpfiles.d/nexus.conf → $TMPFILESDIR/nexus.conf"
maybe install -D -m 0644 \
    "$REPO_ROOT/packaging/tmpfiles.d/nexus.conf" \
    "$TMPFILESDIR/nexus.conf"

log "installing PolicyKit action → $POLKIT_ACTIONS/fi.nexus.policy"
maybe install -D -m 0644 \
    "$REPO_ROOT/packaging/polkit-1/actions/fi.nexus.policy" \
    "$POLKIT_ACTIONS/fi.nexus.policy"

# D-Bus bus policy. Without this, dbus-daemon denies nexusd the
# right to own `fi.nexus1` on the system bus — exit code 1 with
# only `org.freedesktop.DBus.Error.AccessDenied` to go on.
log "installing D-Bus system bus policy → $DBUS_SYSTEM_D/fi.nexus1.conf"
maybe install -D -m 0644 \
    "$REPO_ROOT/packaging/dbus-1/system.d/fi.nexus1.conf" \
    "$DBUS_SYSTEM_D/fi.nexus1.conf"

# Rules file is installed only if no existing one would be clobbered.
if [[ -f "$POLKIT_RULES/50-nexus.rules" ]]; then
    log "$POLKIT_RULES/50-nexus.rules already exists — not overwriting"
else
    log "installing PolicyKit rules → $POLKIT_RULES/50-nexus.rules"
    maybe install -D -m 0644 \
        "$REPO_ROOT/packaging/polkit-1/rules.d/50-nexus.rules" \
        "$POLKIT_RULES/50-nexus.rules"
fi

# ---- Step 5: create /var/lib/nexus ----------------------------------------

log "materializing /var/lib/nexus via systemd-tmpfiles"
maybe systemd-tmpfiles --create "$TMPFILESDIR/nexus.conf"

# ---- Step 6: systemd reload + enable --------------------------------------

log "daemon-reload + enabling nexus.service"
maybe systemctl daemon-reload
maybe systemctl enable nexus.service

# Ask dbus-daemon to re-read its policy tree so the new
# fi.nexus1.conf takes effect without a reboot. `reload dbus` is a
# soft reload; clients are unaffected.
if systemctl is-active --quiet dbus; then
    log "reloading dbus so fi.nexus1.conf takes effect"
    maybe systemctl reload dbus
fi

log "done. Review $CONFDIR/nexus.toml, then start with:"
log "  systemctl start nexus"
log "  journalctl -u nexus -f"
