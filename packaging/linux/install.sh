#!/usr/bin/env sh
# FloatIsland per-user installer for Linux (no root needed).
#   ./install.sh                 install and start, with autostart at login
#   ./install.sh --no-autostart  install without the autostart entry
# Run it from the extracted release archive, or from packaging/linux after `cargo build --release`.
set -eu

DIR=$(cd "$(dirname "$0")" && pwd)
BIN_DIR="$HOME/.local/bin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
AUTOSTART=1
[ "${1:-}" = "--no-autostart" ] && AUTOSTART=0

# The binary: next to this script (release archive) or the cargo build output (repo checkout).
if [ -x "$DIR/floatisland" ]; then
    SRC="$DIR/floatisland"
elif [ -x "$DIR/../../target/release/FloatIsland" ]; then
    SRC="$DIR/../../target/release/FloatIsland"
else
    echo "floatisland binary not found. Build it first: cargo build --release" >&2
    exit 1
fi
LOGO="$DIR/logo.png"
[ -f "$LOGO" ] || LOGO="$DIR/../../assets/logo.png"

# Runtime libraries the binary links against.
MISSING=$(ldd "$SRC" 2>/dev/null | awk '/not found/ {print $1}')
if [ -n "$MISSING" ]; then
    echo "Missing libraries:" >&2
    echo "$MISSING" | sed 's/^/  /' >&2
    echo "Install them, for example:" >&2
    echo "  Ubuntu/Debian: sudo apt install libgtk-4-1 libdbus-1-3" >&2
    echo "  Fedora:        sudo dnf install gtk4 dbus-libs" >&2
    echo "  Arch:          sudo pacman -S gtk4 dbus" >&2
    echo "  openSUSE:      sudo zypper install gtk4" >&2
    echo "If a library is present but too old (glibc), build from source: see README." >&2
    exit 1
fi

pkill -x floatisland 2>/dev/null || true

mkdir -p "$BIN_DIR" "$DATA/applications" "$DATA/icons/hicolor/512x512/apps" "$DATA/floatisland"
install -m 755 "$SRC" "$BIN_DIR/floatisland"
install -m 644 "$LOGO" "$DATA/icons/hicolor/512x512/apps/floatisland.png"
install -m 755 "$DIR/uninstall.sh" "$DATA/floatisland/uninstall.sh"
# Absolute Exec path: ~/.local/bin isn't on PATH for every session/autostart.
sed "s|^Exec=.*|Exec=$BIN_DIR/floatisland|" "$DIR/floatisland.desktop" > "$DATA/applications/floatisland.desktop"
if [ "$AUTOSTART" = 1 ]; then
    mkdir -p "$CONFIG/autostart"
    cp "$DATA/applications/floatisland.desktop" "$CONFIG/autostart/floatisland.desktop"
fi
command -v update-desktop-database >/dev/null && update-desktop-database "$DATA/applications" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$DATA/icons/hicolor" 2>/dev/null || true

nohup "$BIN_DIR/floatisland" >/dev/null 2>&1 &

echo "FloatIsland installed to $BIN_DIR/floatisland and started."
echo "Uninstall: $DATA/floatisland/uninstall.sh"

# Desktop-specific notes.
DESKTOP=$(echo "${XDG_CURRENT_DESKTOP:-}" | tr '[:lower:]' '[:upper:]')
case "$DESKTOP" in
    *GNOME*)
        echo
        echo "GNOME: the tray icon needs the 'AppIndicator and KStatusNotifierItem Support' extension"
        echo "(preinstalled on Ubuntu). Without it, right-click the island to quit." ;;
esac
if [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] && [ "$AUTOSTART" = 1 ]; then
    echo
    echo "Hyprland doesn't run XDG autostart entries. Add this to ~/.config/hypr/hyprland.conf:"
    echo "  exec-once = $BIN_DIR/floatisland"
fi
case "$DESKTOP" in
    *GNOME*) ;;
    *)
        if [ -n "${WAYLAND_DISPLAY:-}" ] && ! ldconfig -p 2>/dev/null | grep -q libgtk4-layer-shell; then
            echo
            echo "Tip: install gtk4-layer-shell for native placement on KDE/Hyprland/Sway"
            echo "(Arch/Fedora: gtk4-layer-shell, Debian 13 / Ubuntu 25.04+: libgtk4-layer-shell0)."
            echo "Without it the island runs through XWayland."
        fi ;;
esac
if ! command -v ddcutil >/dev/null; then
    echo
    echo "Optional: install 'ddcutil' to control external monitor brightness (DDC/CI),"
    echo "and add yourself to the i2c group if your distro needs it: sudo usermod -aG i2c \$USER"
fi
