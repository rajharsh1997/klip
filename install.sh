#!/bin/bash
set -euo pipefail

echo "=== Installing Klip ==="
echo ""

# Configuration — override with env vars
BIN_DIR="${BIN_DIR:-/usr/local/bin}"
ICON_DIR="${ICON_DIR:-/usr/local/share/icons/hicolor}"

# Resolve user home — works both with and without sudo
if [ -n "${SUDO_USER:-}" ]; then
    USER_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
else
    USER_HOME="$HOME"
fi

LOCAL_BIN="${LOCAL_BIN:-$USER_HOME/.local/bin}"
DESKTOP_DIR="${DESKTOP_DIR:-$USER_HOME/.local/share/applications}"
SERVICE_DIR="${SERVICE_DIR:-$USER_HOME/.config/systemd/user}"
LOCAL_ICON_DIR="${LOCAL_ICON_DIR:-$USER_HOME/.local/share/icons/hicolor}"

# Build first (skip if already built — sudo may not have cargo in PATH)
if command -v cargo &>/dev/null; then
    echo "  Building release binaries..."
    cargo build --release --quiet
else
    echo "  Skipping build (cargo not in PATH — assuming binaries already exist)"
fi

install_local() {
    echo "  Installing to user home (~/.local)..."
    mkdir -p "$LOCAL_BIN" "$DESKTOP_DIR" "$SERVICE_DIR"

    install -Dm755 target/release/klipd "$LOCAL_BIN/klipd"
    install -Dm755 target/release/klip-gui "$LOCAL_BIN/klip"

    # Install icons to user local
    for size in 48 128; do
        install -Dm644 "icons/klip-${size}.png" "${LOCAL_ICON_DIR}/${size}x${size}/apps/klip.png"
    done
    install -Dm644 "icons/klip.png" "${LOCAL_ICON_DIR}/256x256/apps/klip.png"
    gtk-update-icon-cache "${LOCAL_ICON_DIR%/*}" 2>/dev/null || true

    # Install .desktop file (for app tray & global shortcut binding)
    install -Dm644 klip.desktop "$DESKTOP_DIR/klip.desktop"
    echo "  ✓ Desktop file: $DESKTOP_DIR/klip.desktop"

    cp klipd.service "$SERVICE_DIR/klipd.service"
    systemctl --user daemon-reload 2>/dev/null || true
    echo "  ✓ Systemd service: $SERVICE_DIR/klipd.service"
    echo "  ✓ Icons installed"

    # Add to PATH if not already
    if [[ ":$PATH:" != *":$LOCAL_BIN:"* ]]; then
        echo "  ⚠  Add $LOCAL_BIN to your PATH:"
        echo "     echo 'export PATH=\"\$PATH:$LOCAL_BIN\"' >> ~/.bashrc"
        echo "     echo 'export PATH=\"\$PATH:$LOCAL_BIN\"' >> ~/.zshrc"
    fi
}

install_system() {
    echo "  Installing system-wide (/usr/local)..."
    sudo install -Dm755 target/release/klipd "$BIN_DIR/klipd"
    sudo install -Dm755 target/release/klip-gui "$BIN_DIR/klip"

    # Install icons to system locations
    for size in 48 128; do
        sudo install -Dm644 "icons/klip-${size}.png" "${ICON_DIR}/${size}x${size}/apps/klip.png"
    done
    sudo install -Dm644 "icons/klip.png" "${ICON_DIR}/256x256/apps/klip.png"
    sudo gtk-update-icon-cache /usr/local/share/icons/hicolor 2>/dev/null || true

    # User files (.desktop, service) go to the actual user's home
    mkdir -p "$DESKTOP_DIR" "$SERVICE_DIR"

    install -Dm644 klip.desktop "$DESKTOP_DIR/klip.desktop"
    echo "  ✓ Desktop file: $DESKTOP_DIR/klip.desktop"

    cp klipd.service "$SERVICE_DIR/klipd.service"
    systemctl --user daemon-reload 2>/dev/null || true
    echo "  ✓ Systemd service: $SERVICE_DIR/klipd.service"
    echo "  ✓ Icons installed"
}

# Check if we have sudo, install system-wide if possible
if command -v sudo &>/dev/null; then
    install_system
else
    install_local
fi

echo ""
echo "=== Installation complete! ==="
echo ""
echo "  Launch the GUI:"
echo "    klip"
echo ""
echo "  The daemon starts automatically when you launch klip."
echo "  For auto-start on login (optional):"
echo "    systemctl --user enable klipd"
echo ""
echo "  Bind a global shortcut (e.g. Ctrl+Alt+V) to 'klip'"
echo "  in your desktop environment's keyboard settings."
echo ""
echo "  To uninstall:"
echo "    rm -f \$(which klip) \$(which klipd)"
echo "    rm -f \$HOME/.local/share/applications/klip.desktop"
echo "    rm -f \$HOME/.config/systemd/user/klipd.service"
echo "    rm -f \$HOME/.local/share/icons/hicolor/*/apps/klip.png"
echo "    sudo rm -f /usr/local/share/icons/hicolor/*/apps/klip.png 2>/dev/null"
echo "    systemctl --user daemon-reload"