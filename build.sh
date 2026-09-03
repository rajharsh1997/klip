#!/bin/bash
set -euo pipefail

echo "=== Building Klip ==="

# Build all workspace crates
cargo build --release

echo ""
echo "Build complete! Binaries:"
echo "  target/release/klipd   - Background daemon"
echo "  target/release/klip    - GUI palette"
echo "  icons/                 - App icon (256x256, 128x128, 48x48)"
echo ""
echo "To install system-wide, run: sudo ./install.sh"