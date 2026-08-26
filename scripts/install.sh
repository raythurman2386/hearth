#!/bin/sh
# Hearth (native) headless installer.
#
#   curl -fsSL https://raw.githubusercontent.com/raythurman2386/hearth/main/scripts/install.sh | sh
#
# Or point at a hub that already has releases published:
#   HEARTH_BASE_URL=http://minis.YOUR-TAILNET.ts.net:27655 sh install.sh
#
# Installs the self-contained native binary (no runtime deps) to
# ~/.hearth/app, puts `hearth` on PATH, and runs it as a local-only
# systemd user service that survives reboots. Tailscale sync is optional
# (see docs/configuration.md). Re-running upgrades in place; ~/.hearth
# state is preserved.
#
# Overrides (if any) go in ~/.hearth/env — typically:
#   HEARTH_TAILNET_HOST=minis.YOUR-TAILNET.ts.net
#   HEARTH_TAILNET_HUB=1
set -eu

BASE="${HEARTH_BASE_URL:-}"
REPO="${HEARTH_RELEASE_REPO:-raythurman2386/hearth}"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "hearth install: macOS desktop packaging is not part of this Linux-focused installer." >&2
    echo "Build/package on a Mac with scripts/package-macos.sh, or wait for a macOS release." >&2
    exit 1
    ;;
  *)
    echo "hearth install: unsupported OS '$os' — only Linux for now." >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "hearth install: unsupported architecture '$arch'." >&2
    exit 1
    ;;
esac

# --- resolve version + artifact URL -----------------------------------------
# Prefer an explicit hub/CDN base (HEARTH_BASE_URL) that serves the updater
# layout (/releases/manifest.json). Otherwise bootstrap from the latest
# GitHub Release for this repo.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if [ -n "$BASE" ]; then
  BASE="${BASE%/}"
  if curl -fsSL "$BASE/releases/manifest.json" -o "$tmp/manifest.json" 2>/dev/null; then
    ver="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp/manifest.json" | head -n1)"
  else
    ver="$(curl -fsSL "$BASE/releases/latest.txt" | tr -d '[:space:]')"
  fi
  [ -n "$ver" ] || { echo "hearth install: could not resolve latest version from $BASE" >&2; exit 1; }
  file="hearth-$ver-$plat-$arch.tar.gz"
  url="$BASE/releases/$file"
else
  echo "resolving latest GitHub release for $REPO…"
  api="https://api.github.com/repos/$REPO/releases/latest"
  curl -fsSL -H "Accept: application/vnd.github+json" "$api" -o "$tmp/release.json"
  ver="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp/release.json" | head -n1)"
  [ -n "$ver" ] || { echo "hearth install: could not parse latest tag from GitHub" >&2; exit 1; }
  file="hearth-$ver-$plat-$arch.tar.gz"
  # Prefer browser_download_url for the tarball + manifest from the JSON.
  url="$(sed -n "s/.*\"browser_download_url\"[[:space:]]*:[[:space:]]*\"\\([^\"]*$file\\)\".*/\\1/p" "$tmp/release.json" | head -n1)"
  [ -n "$url" ] || {
    echo "hearth install: GitHub release v$ver has no $file" >&2
    echo "  (tag a release after the updater packaging workflow lands)" >&2
    exit 1
  }
  manifest_url="$(sed -n 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*manifest\.json\)".*/\1/p' "$tmp/release.json" | head -n1)"
  if [ -n "$manifest_url" ]; then
    curl -fsSL "$manifest_url" -o "$tmp/manifest.json"
  fi
fi

data_root="${HEARTH_DATA_DIR:-$HOME/.hearth}"
app_root="$data_root/app"
dest="$app_root/$ver"

# --- download + verify ------------------------------------------------------
if [ -x "$dest/hearth" ]; then
  echo "hearth $ver already downloaded — relinking."
else
  echo "downloading hearth $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$url" -o "$tmp/$file"

  if [ -f "$tmp/manifest.json" ]; then
    expected="$(sed -n "s/.*\"$file\"[^}]*\"sha256\"[[:space:]]*:[[:space:]]*\"\\([a-fA-F0-9]*\\)\".*/\\1/p" "$tmp/manifest.json" | head -n1)"
    if [ -z "$expected" ]; then
      # Fallback: look for sha256 on the following lines (pretty-printed JSON).
      expected="$(awk -v f="$file" '
        $0 ~ "\"" f "\"" {grab=1}
        grab && /"sha256"/ {
          if (match($0, /"sha256"[[:space:]]*:[[:space:]]*"[a-fA-F0-9]+"/)) {
            s=substr($0, RSTART, RLENGTH)
            sub(/.*"sha256"[[:space:]]*:[[:space:]]*"/, "", s)
            sub(/".*/, "", s)
            print s
            exit
          }
        }
      ' "$tmp/manifest.json")"
    fi
    if [ -z "$expected" ]; then
      echo "hearth install: manifest.json has no sha256 for $file — refusing unverified install" >&2
      exit 1
    fi
    actual="$(sha256sum "$tmp/$file" | awk '{print $1}')"
    if [ "$(echo "$actual" | tr 'A-F' 'a-f')" != "$(echo "$expected" | tr 'A-F' 'a-f')" ]; then
      echo "hearth install: checksum mismatch for $file" >&2
      echo "  expected: $expected" >&2
      echo "  actual:   $actual" >&2
      exit 1
    fi
    echo "checksum ok ($actual)"
  else
    echo "hearth install: no manifest.json — refusing unverified install" >&2
    echo "  publish a release with manifest.json, or set HEARTH_BASE_URL to a hub that serves one" >&2
    exit 1
  fi

  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sf "$app_root/current/hearth" "$HOME/.local/bin/hearth"

# --- service -----------------------------------------------------------------
# The daemon is useful before sync: without HEARTH_TAILNET_HOST it serves the
# local profile. Put tailnet env in ~/.hearth/env and restart to opt in.

service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  cat >"$HOME/.config/systemd/user/hearth.service" <<'UNIT'
[Unit]
Description=Hearth native headless engine
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=%h/.hearth/app/current/hearth headless
Restart=on-failure
RestartSec=5
EnvironmentFile=-%h/.hearth/env

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable hearth
  systemctl --user restart hearth
  service=running
  # Keep the user manager (and the engine) running without an active login.
  loginctl enable-linger "$USER" 2>/dev/null \
    || sudo -n loginctl enable-linger "$USER" 2>/dev/null \
    || echo "warn: could not enable linger — the engine stops when you log out (run: sudo loginctl enable-linger $USER)"
else
  echo "warn: systemd user session not available — run the engine manually with: hearth headless"
fi

# --- agent CLIs ---------------------------------------------------------------
command -v raven >/dev/null 2>&1 || \
  echo "note: Raven CLI not found — install it and put \`raven\` on PATH"
command -v codex >/dev/null 2>&1 || \
  echo "note: Codex CLI not found — install it and put \`codex\` on PATH"
command -v grok >/dev/null 2>&1 || \
  echo "note: Grok CLI not found — install it if you want xAI / Ollama via grok"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to your PATH)' ;;
esac

echo ""
echo "hearth $ver installed$path_hint"
echo ""
case "$service" in
  running)
    echo "the engine is running with the new version (local-only unless HEARTH_TAILNET_HOST is set in ~/.hearth/env)."
    echo "  systemctl --user status hearth    check the service"
    echo ""
    echo "optional tailnet sync (local sessions stay local):"
    echo "  echo HEARTH_TAILNET_HOST=minis.YOUR-TAILNET.ts.net >> ~/.hearth/env"
    echo "  echo HEARTH_TAILNET_HUB=1 >> ~/.hearth/env   # on the always-on host only"
    echo "  systemctl --user restart hearth"
    echo ""
    echo "on the hub, publish releases with:"
    echo "  hearth release publish --from github"
    ;;
  manual)
    echo "next: run the local-only engine with \`hearth headless\`."
    echo "optional sync: set HEARTH_TAILNET_HOST before starting the engine."
    ;;
esac
