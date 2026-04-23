# Nexus Development Container
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# Install system dependencies (host-native, for running tests locally)
RUN apt-get update && apt-get install -y \
    # D-Bus development
    libdbus-1-dev \
    dbus \
    # systemd for sd-bus (used by some D-Bus libs)
    libsystemd-dev \
    # libudev — required by the `udev` crate used in nexus-interface-monitor
    libudev-dev \
    # GPS daemon (for testing gpsd backend)
    gpsd \
    libgps-dev \
    # Bluetooth (for testing)
    bluez \
    libbluetooth-dev \
    # Build tools
    pkg-config \
    cmake \
    ninja-build \
    # C++ compiler for examples
    g++ \
    # Debugging
    gdb \
    strace \
    # SELinux tools (optional)
    selinux-utils \
    # Utilities
    jq \
    socat \
    netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*

# ---------- Yocto SDK for aarch64 cross-compilation ----------
# Install the Poky SDK (armv8a / aarch64) into /opt/astrax.
# The installer is large (~2.3 GB) so COPY + RUN + rm keeps exactly one
# extra layer instead of leaving the installer in the final image.
COPY .devcontainer/armv8a-sdk.sh /tmp/armv8a-sdk.sh
RUN chmod +x /tmp/armv8a-sdk.sh \
    && /tmp/armv8a-sdk.sh -y -d /opt/astrax \
    && rm /tmp/armv8a-sdk.sh

# llvm-tools-preview is required by cargo-llvm-cov; clippy and rustfmt are
# already present in the base image so only the coverage component is added.
# Also add the aarch64 Rust target for cross-compilation.
RUN rustup component add llvm-tools-preview \
    && rustup target add aarch64-unknown-linux-gnu

# Install cargo-binstall to download pre-built tool binaries instead of
# compiling from source, keeping image builds fast and versions pinned.
RUN curl -sSfL https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash

# cargo-deny: license, advisory, and supply-chain checks (deny.toml)
RUN cargo binstall --no-confirm --version 0.19.0 cargo-deny
# cargo-llvm-cov: source-based code coverage via LLVM instrumentation
RUN cargo binstall --no-confirm --version 0.8.5 cargo-llvm-cov

# Create runtime directory for nexus socket
RUN mkdir -p /run/nexus && chmod 755 /run/nexus

# Set working directory
WORKDIR /workspace

# Default command
CMD ["sleep", "infinity"]
