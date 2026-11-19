#!/usr/bin/env bash
#
# Build YourFS release binaries for multiple architectures and package them
# into dist/*.tar.gz with a SHA256SUMS.txt manifest.
#
# This is the pure-Rust stack (fuser + nfs-rs): nothing links libfuse/libnfs,
# so the only external requirements are a Rust target per architecture plus a
# matching C cross-toolchain for the bundled SQLite (built by the `cc` crate).
#
# Locally buildable set (needs the cross gcc packages + rustup targets):
#   x86_64-unknown-linux-gnu
#   x86_64-unknown-linux-musl   (fully static, no interpreter)
#   aarch64-unknown-linux-gnu   (ARM64, dynamic glibc)
#   armv7-unknown-linux-gnueabihf (ARM32 hard-float, dynamic glibc)
#
# ARM *static* (musl) builds are intentionally NOT produced here because Ubuntu
# 20.04 ships no aarch64/armv7 musl cross toolchain; the GitHub Actions release
# workflow (.github/workflows/release.yml) builds those via `cross`.
#
# Prerequisites (Debian/Ubuntu):
#   rustup target add aarch64-unknown-linux-gnu armv7-unknown-linux-gnueabihf
#   sudo apt-get install -y musl-gcc gcc-aarch64-linux-gnu gcc-arm-linux-gnueabihf
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)"
DIST="$ROOT/dist"

# Cross toolchains (empty for native x86_64 targets).
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
export AR_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ar
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc

# rustup target triple -> release asset architecture tag
TARGETS=(
  "x86_64-unknown-linux-gnu:linux-x86_64-gnu"
  "x86_64-unknown-linux-musl:linux-x86_64-musl"
  "aarch64-unknown-linux-gnu:linux-aarch64-gnu"
  "armv7-unknown-linux-gnueabihf:linux-armv7-gnueabihf"
)

mkdir -p "$DIST"
rm -f "$DIST"/*.tar.gz "$DIST/SHA256SUMS.txt"

for entry in "${TARGETS[@]}"; do
  triple="${entry%%:*}"
  archtag="${entry##*:}"
  echo "==> building $triple"
  cargo build --release --target "$triple"

  stage="$(mktemp -d)"
  cp "target/$triple/release/yourfs" "target/$triple/release/yourfs-tool" "$stage/"
  pkg="$DIST/yourfs-$VERSION-$archtag.tar.gz"
  # Flattened archive: yourfs + yourfs-tool at the tar root (matches v1 layout).
  tar -C "$stage" -czf "$pkg" yourfs yourfs-tool
  rm -rf "$stage"
  echo "    -> $pkg"
done

echo "==> writing SHA256SUMS.txt"
(
  cd "$DIST"
  sha256sum yourfs-*.tar.gz >SHA256SUMS.txt
)

echo "Done. Artifacts in $DIST:"
ls -1 "$DIST"
