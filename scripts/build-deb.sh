#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo is not installed" >&2
  exit 1
}
command -v dpkg-deb >/dev/null 2>&1 || {
  echo "error: dpkg-deb is not installed" >&2
  exit 1
}
command -v dpkg-architecture >/dev/null 2>&1 || {
  echo "error: dpkg-architecture is not installed" >&2
  exit 1
}

arch="$(dpkg-architecture -qDEB_HOST_ARCH)"
if [[ "$arch" != "amd64" ]]; then
  echo "error: this builder is intended for amd64; detected: $arch" >&2
  echo "Use an x86_64 Linux build host for the friend's test package." >&2
  exit 1
fi

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)"
if [[ -z "$version" ]]; then
  echo "error: could not determine version from Cargo.toml" >&2
  exit 1
fi

# Build both the GUI and its control CLI from the locked dependency graph.
cargo build --release --locked -p cmuxp-app -p cmuxp-ctl

for bin in cmuxp cmuxpctl; do
  if [[ ! -x "target/release/$bin" ]]; then
    echo "error: missing target/release/$bin" >&2
    exit 1
  fi
done

pkgdir="$(mktemp -d)"
trap 'rm -rf "$pkgdir"' EXIT

install -Dm755 target/release/cmuxp    "$pkgdir/usr/bin/cmuxp"
install -Dm755 target/release/cmuxpctl "$pkgdir/usr/bin/cmuxpctl"
install -Dm644 data/dev.cmuxp.App.desktop \
  "$pkgdir/usr/share/applications/dev.cmuxp.App.desktop"
install -Dm644 data/icons/dev.cmuxp.App.svg \
  "$pkgdir/usr/share/icons/hicolor/scalable/apps/dev.cmuxp.App.svg"

mkdir -p "$pkgdir/DEBIAN"
cat > "$pkgdir/DEBIAN/control" <<CONTROL
Package: cmuxp
Version: $version
Section: utils
Priority: optional
Architecture: amd64
Maintainer: Shivaay
Depends: libc6 (>= 2.38), libgtk-4-1 (>= 4.14), libvte-2.91-gtk4-0, libwebkitgtk-6.0-4
Description: Linux-native terminal for running coding agents side by side
 cmuxp is a GTK4/VTE terminal environment for parallel coding workflows,
 with workspaces, split panes, tabs, a built-in WebKit browser and an
 ephemeral per-workspace network sandbox.
CONTROL

mkdir -p dist
out="dist/cmuxp_${version}_${arch}.deb"
dpkg-deb --build --root-owner-group "$pkgdir" "$out" >/dev/null

echo "Built: $out"
dpkg-deb --info "$out"
