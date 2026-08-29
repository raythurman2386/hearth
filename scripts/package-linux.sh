#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/hearth-<version>-linux-<arch>.tar.gz
# containing the binary, the .desktop entry, the icon, and an install.sh that
# installs into the managed layout (~/.hearth/app/<ver> + current symlink) so
# `hearth update` works after install. Desktop entry + icon still land in XDG.
#
# Usage: scripts/package-linux.sh
# Env:
#   PROFILE=debug     — fast unoptimized package (CI smoke); default release.
#   PREBUILT_BIN=path — skip cargo build; package this binary instead (CI).
#   VERSION=x.y.z     — override version (default: Cargo.toml workspace version).
#   ARCH=x86_64|aarch64 — override the target arch (default: uname -m). Use
#                         when cross-compiling (e.g. CI builds an aarch64 binary
#                         on an x86_64 runner via cross).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="${ARCH:-$(uname -m)}"
case "$ARCH" in
  x86_64 | amd64) ARCH=x86_64 ;;
  aarch64 | arm64) ARCH=aarch64 ;;
esac
VERSION="${VERSION:-$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')}"
VERSION="${VERSION#v}"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/hearth-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ -n "${PREBUILT_BIN:-}" ]]; then
  BIN="$PREBUILT_BIN"
  [[ -x "$BIN" || -f "$BIN" ]] || {
    echo "package-linux: PREBUILT_BIN not found: $BIN" >&2
    exit 1
  }
elif [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p hearth
  BIN="$ROOT/target/release/hearth"
else
  cargo build -p hearth
  BIN="$ROOT/target/debug/hearth"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/hearth"
install -m 644 "$ROOT/dist/hearth.desktop" "$STAGE/hearth.desktop"
install -m 644 "$ROOT/dist/hearth.png" "$STAGE/hearth.png"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Hearth into the managed layout (~/.hearth/app/<ver>) so self-update works.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION="$(basename "$HERE" | sed -n 's/^hearth-\(.*\)-linux-.*/\1/p')"
if [[ -z "$VERSION" ]]; then
  echo "install: could not parse version from $(basename "$HERE")" >&2
  exit 1
fi
data_root="${HEARTH_DATA_DIR:-$HOME/.hearth}"
app_root="$data_root/app"
dest="$app_root/$VERSION"
mkdir -p "$dest"
install -m 755 "$HERE/hearth" "$dest/hearth"
ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sfn "$app_root/current/hearth" "$HOME/.local/bin/hearth"
if [[ -f "$HERE/hearth.desktop" ]]; then
  install -Dm644 "$HERE/hearth.desktop" "$HOME/.local/share/applications/hearth.desktop"
fi
if [[ -f "$HERE/hearth.png" ]]; then
  install -Dm644 "$HERE/hearth.png" \
    "$HOME/.local/share/icons/hicolor/1024x1024/apps/hearth.png"
fi
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
echo "Installed hearth $VERSION (managed: $app_root/current)."
echo "Make sure ~/.local/bin is on your PATH."
INSTALL
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
