#!/usr/bin/env bash
# Runs inside docker.io/library/ubuntu:22.04 (see build-packages.sh).
# /src is the read-only checkout, /out receives the packages, /build holds
# the cargo target directory between runs.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
export CARGO_TARGET_DIR=/build/target
export PATH="/root/.cargo/bin:$PATH"

apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  build-essential ca-certificates curl file dpkg-dev pkg-config patchelf desktop-file-utils \
  libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev libdbus-1-dev librsvg2-dev \
  libfuse2 squashfs-tools zstd >/dev/null

if ! command -v cargo >/dev/null; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
fi
rustup update stable >/dev/null
command -v cargo-deb >/dev/null || cargo install --locked cargo-deb
command -v cargo-generate-rpm >/dev/null || cargo install --locked cargo-generate-rpm

export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=/root/.cargo=cargo --remap-path-prefix=/src=inxtdrive"

cd /src
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
cargo build --locked --release -p inxtdrive

cargo deb -p inxtdrive --no-build --no-strip -o /out/
cargo generate-rpm -p crates/inxtdrive --target-dir "$CARGO_TARGET_DIR" -o /out/

bash /src/scripts/linux/make-appimage.sh "$version" /out

chown "${HOST_UID:-0}:${HOST_GID:-0}" /out/*"$version"* || true
