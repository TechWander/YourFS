#!/usr/bin/env bash
#
# build-musl-static.sh — produce a FULLY STATIC musl build of yourfs.
#
# yourfs now runs on a pure-Rust stack (fuser for FUSE, nfs-rs for NFS), so the
# old libfuse.a / libnfs.a cross-compilation steps and build.rs are gone. The
# only C that still gets compiled is the bundled SQLite inside `rusqlite`, via
# the `cc` crate, so a musl C toolchain is the only external build requirement
# besides the Rust musl target itself. Rust links the musl CRT statically by
# default for this target, giving a binary with zero dynamic dependencies.
#
# Result: target/x86_64-unknown-linux-musl/release/yourfs{,-tool} with no
# dynamic dependencies (verified below with `ldd ... => statically linked`).
#
# Requirements on the build host: rustup/cargo with the x86_64-unknown-linux-musl
# target and musl-tools (musl-gcc).
#
# Note: even a fully static yourfs still needs the FUSE *runtime* on the target
# machine — the kernel `fuse` module, /dev/fuse, and the `fusermount` helper
# (from the `fuse` package). Those cannot be bundled into a single ELF.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${TARGET:-x86_64-unknown-linux-musl}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing tool: $1" >&2; exit 1; }; }
need cargo; need musl-gcc

rustup target add "$TARGET" >/dev/null 2>&1 || true

# ---- yourfs (musl, fully static) ----
cd "$ROOT"
cargo build --release --target "$TARGET"

B="target/$TARGET/release"
echo
echo "Artifacts:"
ls -l "$B/yourfs" "$B/yourfs-tool"
echo "Verify:"
file "$B/yourfs" | sed 's/.*: //'
ldd "$B/yourfs" 2>&1 | head -1   # => "statically linked"
