#!/usr/bin/env sh
# Removes a per-user FloatIsland install. Keeps ~/.config/floatisland (your position setting)
# unless you pass --purge.
set -eu

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"

pkill -x floatisland 2>/dev/null || true
rm -f "$HOME/.local/bin/floatisland" \
      "$DATA/applications/floatisland.desktop" \
      "$DATA/icons/hicolor/512x512/apps/floatisland.png" \
      "$CONFIG/autostart/floatisland.desktop"
rm -rf "$DATA/floatisland"
[ "${1:-}" = "--purge" ] && rm -rf "$CONFIG/floatisland"
echo "FloatIsland removed."
