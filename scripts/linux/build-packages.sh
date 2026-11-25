#!/usr/bin/env bash
# Builds the Linux packages into dist/.
#
#   scripts/linux/build-packages.sh            all four: Arch, .deb, .rpm, AppImage
#   scripts/linux/build-packages.sh arch       only the Arch package (native makepkg)
#   scripts/linux/build-packages.sh portable   only .deb, .rpm and AppImage
#
# The Arch package is built natively. The other three are built inside an
# Ubuntu 22.04 container (podman or docker) so they link against an older
# glibc and run on Debian 12, Ubuntu 22.04, Fedora, openSUSE and newer.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)"
dist="$root/dist"
what="${1:-all}"
mkdir -p "$dist"

build_arch() {
  command -v makepkg >/dev/null || { echo "makepkg not found (this step needs Arch Linux)"; exit 1; }
  local work="$root/target/arch-pkg"
  rm -rf "$work"
  mkdir -p "$work"
  sed "s/^pkgver=.*/pkgver=$version/" "$root/packaging/arch/PKGBUILD" > "$work/PKGBUILD"
  (cd "$work" && INXT_ROOT="$root" makepkg -f --syncdeps --noconfirm)
  cp "$work"/inxtdrive-"$version"-*.pkg.tar.zst "$dist/"
}

build_portable() {
  local engine=""
  for e in podman docker; do
    if command -v "$e" >/dev/null; then engine="$e"; break; fi
  done
  [ -n "$engine" ] || { echo "Needs podman or docker (sudo pacman -S podman)"; exit 1; }
  # Root in a rootless podman container is already the calling user, so only
  # docker needs the packages handed back to the host user.
  local owner=()
  [ "$engine" = docker ] && owner=(-e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)")
  "$engine" run --rm \
    -v "$root:/src:ro" \
    -v "$dist:/out" \
    -v inxtdrive-jammy-cargo:/root/.cargo \
    -v inxtdrive-jammy-rustup:/root/.rustup \
    -v inxtdrive-jammy-build:/build \
    "${owner[@]}" \
    docker.io/library/ubuntu:22.04 \
    bash /src/scripts/linux/build-in-container.sh
}

case "$what" in
  all) build_arch; build_portable ;;
  arch) build_arch ;;
  portable) build_portable ;;
  *) echo "unknown target: $what (all, arch, portable)"; exit 1 ;;
esac

echo
echo "Packages in $dist:"
ls -1 "$dist" | grep -iE "inxtdrive.*$version" || true
