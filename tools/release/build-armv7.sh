#!/usr/bin/env bash
# Builds pm-node for 32-bit Raspberry Pi OS (armv7, hard float)
# in a Debian bookworm container with the GNU cross toolchain and armhf ALSA
# headers, the same way the Dockerfile cross-builds arm64. Needs Docker.
# Output: target/armv7-unknown-linux-gnueabihf/release/pm-node
set -euo pipefail
cd "$(dirname "$0")/../.."
mkdir -p "${CARGO_HOME_CACHE:-$PWD/.cargo-armv7}"
docker run --rm \
  -v "$PWD":/src -w /src \
  -v "${CARGO_HOME_CACHE:-$PWD/.cargo-armv7}":/usr/local/cargo/registry \
  -e CARGO_INCREMENTAL=0 -e CARGO_TERM_COLOR=always \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -e PM_NODE_VERSION="${PM_NODE_VERSION:-}" \
  rust:1-bookworm bash -euo pipefail -c '
    dpkg --add-architecture armhf
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      gcc-arm-linux-gnueabihf libc6-dev-armhf-cross libasound2-dev:armhf cmake pkg-config >/dev/null
    rustup target add armv7-unknown-linux-gnueabihf
    export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
           CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc \
           AR_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ar \
           PKG_CONFIG_ALLOW_CROSS=1 \
           PKG_CONFIG_PATH=/usr/lib/arm-linux-gnueabihf/pkgconfig
    cargo build --release --locked -p pm-node --features alsa \
      --target armv7-unknown-linux-gnueabihf
    chown -R "$HOST_UID:$HOST_GID" target /usr/local/cargo/registry
  '
