#!/usr/bin/env bash
# Build the dedicated desktop platform: algo-server + algo-desktop + static UI.
# Also produces the updater release asset + SHA256SUMS in dist/.
# Output: dist/ (portable folder + algo-desktop-<os>-<arch>.zip + SHA256SUMS)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIST="$ROOT/dist"

echo "[1/4] building algo-server (release)"
cargo build --release -p algo-server --manifest-path "$ROOT/Cargo.toml"

echo "[2/4] building algo-desktop (release)"
cargo build --release --manifest-path "$ROOT/desktop/Cargo.toml"

echo "[3/4] assembling $DIST"
mkdir -p "$DIST/static"
cp "$ROOT/target/release/algo-server" "$DIST/"
cp -R "$ROOT/crates/server/static/." "$DIST/static/"
cp "$ROOT/desktop/target/release/algo-desktop" "$DIST/"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$OS" in
  darwin) OS=macos ;;
esac
ARCH="$(uname -m)"
case "$ARCH" in
  amd64) ARCH=x86_64 ;;
  arm64) ARCH=aarch64 ;;
esac
ASSET="algo-desktop-${OS}-${ARCH}.zip"

echo "[4/4] packaging update asset $ASSET"
if command -v python3 >/dev/null 2>&1; then
  ( cd "$DIST" && python3 - "$ASSET" <<'PY'
import os, sys, zipfile
name = sys.argv[1]
with zipfile.ZipFile(name, "w", zipfile.ZIP_DEFLATED) as z:
    for root, _dirs, files in os.walk("."):
        for f in files:
            p = os.path.join(root, f)
            if os.path.abspath(p) in (os.path.abspath(name), os.path.abspath("SHA256SUMS")):
                continue
            z.write(p, os.path.relpath(p, "."))
PY
  )
else
  ( cd "$DIST" && zip -r "$ASSET" . -x "$ASSET" -x "SHA256SUMS" )
fi

if command -v sha256sum >/dev/null 2>&1; then
  ( cd "$DIST" && sha256sum "$ASSET" > SHA256SUMS )
else
  ( cd "$DIST" && shasum -a 256 "$ASSET" > SHA256SUMS )
fi

echo "done -> $DIST (folder + $ASSET + SHA256SUMS)"
