#!/bin/bash
set -euo pipefail

echo "=== Installing Klip ==="
echo ""

# Configuration — override with env vars
BIN_DIR="${BIN_DIR:-/usr/local/bin}"
LOCAL_BIN="${LOCAL_BIN:-$HOME/.local/bin}"
DESKTOP_DIR="${DESKTOP_DIR:-$HOME/.local/share/applications}"
SERVICE_DIR="${SERVICE_DIR:-$HOME/.config/systemd/user}"

# Build first
echo "  Building release binaries..."
cargo build --release --quiet

install_local() {
    echo "  Installing to user home (~/.local)..."
    mkdir -p "$LOCAL_BIN" "$DESKTOP_DIR" "$SERVICE_DIR"

    install -Dm755 target/release/klipd "$LOCAL_BIN/klipd"
    install -Dm755 target/release/klip-gui "$LOCAL_BIN/klip-gui"

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
    sudo install -Dm755 target/release/klip-gui "$BIN_DIR/klip-gui"
}

# Check if we have sudo, install system-wide if possible
if command -v sudo &>/dev/null; then
    install_system
else
    install_local
fi

# Install .desktop file (for global shortcut binding)
mkdir -p "$DESKTOP_DIR"
cat > "$DESKTOP_DIR/klip-gui.desktop" <<EOF
[Desktop Entry]
Name=Klip Clipboard Manager
Comment=Show clipboard history palette
Exec=klip-gui
Icon=edit-paste-symbolic
Type=Application
Categories=Utility;
NoDisplay=true
EOF
echo "  ✓ Desktop file: $DESKTOP_DIR/klip-gui.desktop"

# Install systemd user service
mkdir -p "$SERVICE_DIR"
cp klipd.service "$SERVICE_DIR/klipd.service"
systemctl --user daemon-reload 2>/dev/null || true
echo "  ✓ Systemd service: $SERVICE_DIR/klipd.service"

echo ""
echo "=== Installation complete! ==="
echo ""
echo "  Start the daemon:"
echo "    systemctl --user start klipd"
echo "    systemctl --user enable klipd"
echo ""
echo "  Launch the GUI:"
echo "    klip-gui"
echo ""
echo "  Bind a global shortcut (e.g. Ctrl+Alt+V) to 'klip-gui'"
echo "  in your desktop environment's keyboard settings."
echo ""
echo "  To uninstall:"
echo "    rm -f \$(which klipd) \$(which klip-gui)"
echo "    rm -f \$HOME/.local/share/applications/klip-gui.desktop"
echo "    rm -f \$HOME/.config/systemd/user/klipd.service"
echo "    systemctl --user daemon-reload"