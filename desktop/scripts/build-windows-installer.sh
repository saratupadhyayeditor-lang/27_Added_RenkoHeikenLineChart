#!/usr/bin/env bash
# Build the Windows installer (AlgoTradingSetup.exe) from Linux by
# cross-compiling with cargo-xwin and packaging with NSIS (makensis).
#
# Requires: cargo-xwin, clang-cl + lld-link + llvm-lib + llvm-rc (llvm),
#           makensis, rustup target x86_64-pc-windows-msvc.
#
# Output: dist-windows/AlgoTradingSetup.exe
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
STAGE="$ROOT/dist-windows"
TARGET="x86_64-pc-windows-msvc"

# Statically link the MSVC CRT so the app has no VC++ Redistributable dependency.
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"

echo "[1/4] cross-compiling algo-server.exe ($TARGET)"
cargo xwin build --release --target "$TARGET" -p algo-server --manifest-path "$ROOT/Cargo.toml"

echo "[2/4] cross-compiling algo-desktop.exe ($TARGET)"
cargo xwin build --release --target "$TARGET" --manifest-path "$ROOT/desktop/Cargo.toml"

echo "[3/4] staging $STAGE"
mkdir -p "$STAGE/static"
cp "$ROOT/target/$TARGET/release/algo-server.exe" "$STAGE/"
cp "$ROOT/desktop/target/$TARGET/release/algo-desktop.exe" "$STAGE/"
cp -R "$ROOT/crates/server/static/." "$STAGE/static/"

echo "[4/4] building installer with makensis"
makensis -DSTAGE="$STAGE" -DOUTDIR="$STAGE" "$ROOT/desktop/scripts/installer-windows.nsi"

echo "done -> $STAGE/AlgoTradingSetup.exe"
