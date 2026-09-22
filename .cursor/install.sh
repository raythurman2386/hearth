#!/usr/bin/env bash
# Idempotent Cloud Agent bootstrap for Hearth (Rust + gpui).
#
# gpui (the wingleeio/zed fork) links fontconfig/freetype/xkbcommon/wayland/x11/
# xcb plus GL/EGL and glib/pango/cairo/gtk on Linux; the base image does not
# ship the -dev packages. This mirrors the deps in .github/workflows/ci.yml and
# adds a software GL/Vulkan stack + Xvfb so the headed UI can also run offscreen.
set -euo pipefail

cd "$(dirname "$0")/.."

SYS_DEPS=(
  # gpui link-time deps (kept in sync with .github/workflows/ci.yml)
  libfontconfig1-dev libfreetype6-dev libxkbcommon-dev
  libwayland-dev libx11-dev libxcb1-dev libxcb-render0-dev
  libxcb-shape0-dev libxcb-xfixes0-dev libxcb-xkb-dev
  libxcb-icccm4-dev libxcb-image0-dev libxcb-keysyms1-dev
  libxcb-randr0-dev libxcb-render-util0-dev libxcb-shm0-dev
  libxcb-util-dev libxcb-xinerama0-dev libxcb-xinput-dev
  libxkbcommon-x11-dev libegl1-mesa-dev libgl1-mesa-dev
  libgles2-mesa-dev libglib2.0-dev libpango1.0-dev
  libcairo2-dev libgdk-pixbuf-2.0-dev libgtk-3-dev
  # offscreen rendering for the headed app (software GL/Vulkan + virtual X)
  mesa-vulkan-drivers libvulkan1 vulkan-tools xvfb
)

echo "▸ installing system dependencies (apt)…"
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${SYS_DEPS[@]}"

# Warm the workspace build cache so fresh agents start with a populated
# target/ (gpui is a large first build). Incremental, so this is cheap on
# re-run. Uses the pinned stable toolchain from rust-toolchain.toml.
echo "▸ building hearth (debug)…"
cargo build -p hearth

echo "✓ hearth environment ready"
