#!/bin/sh
# Run Job Command Center from source with one command (macOS and Linux).
# Installs what is missing: the Rust toolchain, and on Linux the build libraries.
# After the first build (a few minutes) later starts take seconds.
set -eu
cd "$(dirname "$0")/.."

if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
  PATH="$HOME/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "Installing the Rust toolchain (rustup)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  PATH="$HOME/.cargo/bin:$PATH"
fi

case "$(uname -s)" in
  Linux)
    if command -v apt-get >/dev/null 2>&1; then
      pkgs="build-essential pkg-config libxkbcommon-dev libfontconfig1-dev libdbus-1-dev libwayland-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev"
      missing=""
      for p in $pkgs; do dpkg -s "$p" >/dev/null 2>&1 || missing="$missing $p"; done
      if [ -n "$missing" ]; then
        echo "Installing build libraries (needs your password):$missing"
        sudo apt-get update -qq && sudo apt-get install -y -qq $missing
      fi
    elif command -v dnf >/dev/null 2>&1; then
      sudo dnf install -y -q gcc gcc-c++ pkgconf-pkg-config libxkbcommon-devel fontconfig-devel dbus-devel wayland-devel libXcursor-devel libXrandr-devel libXi-devel mesa-libGL-devel
    else
      echo "Install your distribution's packages for: xkbcommon, fontconfig, dbus, wayland, Xcursor, Xrandr, Xi, OpenGL (development headers)."
    fi
    ;;
  Darwin)
    xcode-select -p >/dev/null 2>&1 || { echo "Installing Apple's command line tools (a dialog will open; run this again when it finishes)."; xcode-select --install; exit 1; }
    ;;
esac

echo "Building and starting Job Command Center (the first build takes a few minutes)..."
exec cargo run --release --locked -- "$@"
