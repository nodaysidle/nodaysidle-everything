#!/bin/bash
# Installs Craft Anything for the current user: binary, launcher, app-menu entry and icon.
# Remove with: packaging/install.sh --uninstall
set -euo pipefail
cd "$(dirname "$0")/.."

bin="$HOME/.local/bin"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"

if [[ ${1-} == --uninstall ]]; then
  rm -f "$bin/craft-anything" "$bin/craft-anything-app" "$apps/craft-anything.desktop" "$icons/craft-anything.svg"
  echo "Removed. Run data stays in ${XDG_DATA_HOME:-$HOME/.local/share}/craft-anything."
  exit 0
fi

# A release tarball ships the binary beside this folder; a git checkout builds it.
if [[ -x craft-anything ]]; then
  built=craft-anything
else
  cargo build --release -p craft-app
  built=target/release/craft-anything
fi
install -Dm755 "$built" "$bin/craft-anything"
install -Dm755 packaging/craft-anything-app "$bin/craft-anything-app"
install -Dm644 packaging/craft-anything.desktop "$apps/craft-anything.desktop"
install -Dm644 packaging/craft-anything.svg "$icons/craft-anything.svg"
update-desktop-database "$apps" 2>/dev/null || true
gtk-update-icon-cache -q "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" 2>/dev/null || true
echo "Installed. Open 'Craft Anything' from the app menu, or run: craft-anything-app"
