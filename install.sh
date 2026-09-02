#!/bin/bash
set -euo pipefail

echo "=== Installing Klip ==="

BIN_DIR="${BIN_DIR:-/usr/local/bin}"

# Build first
cargo build --release

# Install binaries
sudo install -Dm755 target/release/klipd "$BIN_DIR/klipd"
sudo install -Dm755 target/release/klip-gui "$BIN_DIR/klip-gui"

# Install .desktop file so KDE/GNOME can launch it via global shortcut
DESKTOP_DIR="$HOME/.local/share/applications"
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

# Install systemd user service
mkdir -p "$HOME/.config/systemd/user"
cp klipd.service "$HOME/.config/systemd/user/klipd.service"
systemctl --user daemon-reload

echo ""
echo "Installation complete!"
echo ""
echo "Start the daemon:"
echo "  systemctl --user start klipd"
echo "  systemctl --user enable klipd"
echo ""
echo "To launch the GUI, bind a global shortcut to:"
echo "  klip-gui"
echo ""
echo "On KDE: System Settings → Shortcuts → Custom Shortcuts → New → Command"
echo "  Command: klip-gui"
echo "  Shortcut: e.g. Meta+V or Ctrl+Alt+V"