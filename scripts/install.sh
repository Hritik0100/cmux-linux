#!/usr/bin/env bash
# Build cmuxp in release mode and install it for the current user
# (binaries, app icon, and desktop entry so the launcher/taskbar shows the logo).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release

install -Dm755 target/release/cmuxp target/release/cmuxpctl -t "$HOME/.local/bin/"
install -Dm644 data/icons/dev.cmuxp.App.svg -t "$HOME/.local/share/icons/hicolor/scalable/apps/"
install -Dm644 data/dev.cmuxp.App.desktop -t "$HOME/.local/share/applications/"

command -v update-desktop-database >/dev/null && update-desktop-database "$HOME/.local/share/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

# Ubuntu (23.10+, default from 24.04) restricts unprivileged user namespaces via
# AppArmor, which breaks the per-workspace network sandbox (it needs CAP_NET_ADMIN
# to bring `lo` up inside the namespace). A named AppArmor profile for cmuxp skips
# that restriction. Installing it needs root, so it is best-effort.
if [ -d /etc/apparmor.d ] && command -v apparmor_parser >/dev/null; then
  if [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = "1" ]; then
    if sudo install -Dm644 data/cmuxp.apparmor /etc/apparmor.d/cmuxp \
       && sudo apparmor_parser -r /etc/apparmor.d/cmuxp; then
      echo "Installed AppArmor profile /etc/apparmor.d/cmuxp (network sandbox enabled)."
    else
      echo
      echo "NOTE: this system restricts unprivileged user namespaces, which disables the"
      echo "per-workspace network sandbox until one of these is run (needs root):"
      echo "  sudo install -Dm644 data/cmuxp.apparmor /etc/apparmor.d/cmuxp && sudo apparmor_parser -r /etc/apparmor.d/cmuxp"
      echo "  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0"
    fi
  fi
fi

echo "Installed. Make sure ~/.local/bin is on your PATH, then run: cmuxp"
