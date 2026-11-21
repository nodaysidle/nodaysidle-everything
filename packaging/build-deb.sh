#!/bin/bash
# Builds a .deb from the release binary without needing dpkg: packaging/build-deb.sh [output-dir]
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
out=$(realpath "${1:-.}")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo build --release --locked -p craft-app
install -Dm755 target/release/craft-anything "$work/data/usr/bin/craft-anything"
strip "$work/data/usr/bin/craft-anything"
install -Dm755 packaging/craft-anything-app "$work/data/usr/bin/craft-anything-app"
install -Dm644 packaging/craft-anything.desktop "$work/data/usr/share/applications/craft-anything.desktop"
install -Dm644 packaging/craft-anything.svg "$work/data/usr/share/icons/hicolor/scalable/apps/craft-anything.svg"
install -Dm644 LICENSE "$work/data/usr/share/doc/craft-anything/copyright"

# The oldest glibc the binary runs on is whatever it was linked against here.
glibc=$(objdump -T target/release/craft-anything | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 | cut -d_ -f2)
mkdir "$work/control"
cat >"$work/control/control" <<CONTROL
Package: craft-anything
Version: $version
Section: devel
Priority: optional
Architecture: amd64
Depends: libc6 (>= $glibc), libgcc-s1, curl
Suggests: chromium | chromium-browser | google-chrome-stable
Installed-Size: $(du -sk "$work/data" | cut -f1)
Maintainer: nodaysidle <nodaysidle@users.noreply.github.com>
Homepage: https://github.com/nodaysidle/nodaysidle-everything
Description: Rebuild what you can see with your own coding agent
 Studies a website, desktop app, phone app, game or file, has your installed
 coding agent rebuild it, checks how close the result is, and refines it.
 Clean-room by default.
CONTROL

tar --owner=0 --group=0 --numeric-owner -C "$work/control" -cJf "$work/control.tar.xz" ./control
tar --owner=0 --group=0 --numeric-owner -C "$work/data" -cJf "$work/data.tar.xz" .
echo 2.0 >"$work/debian-binary"
deb="$out/craft-anything_${version}_amd64.deb"
rm -f "$deb"
# Member order matters to dpkg: debian-binary, control, data.
(cd "$work" && ar rc "$deb" debian-binary control.tar.xz data.tar.xz)
echo "$deb"
