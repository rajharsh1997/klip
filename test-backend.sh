#!/bin/bash
# Klip Backend Test Script
# Usage: ./test-backend.sh [kde|gnome|watch]

set -euo pipefail

KLIP_DIR="$(cd "$(dirname "$0")" && pwd)"
SOCKET="$HOME/.local/share/klip/klip.sock"

build() {
    echo "=== Building ==="
    cargo build --quiet 2>&1
}

test_backend() {
    local backend="$1"
    echo "=== Testing backend: $backend ==="
    echo ""

    # Kill old daemon
    pkill -x "klip" 2>/dev/null || true
    rm -f "$SOCKET"
    sleep 0.5

    # Build test IPC client if needed
    if [ ! -f "$KLIP_DIR/tmp/test_ipc2" ]; then
        mkdir -p "$KLIP_DIR/tmp"
        cat > /tmp/_test_ipc2.rs << 'EOF'
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
fn main() {
    let stream = UnixStream::connect(std::env::args().nth(1).unwrap_or_else(|| "/home/harsh/.local/share/klip/klip.sock".into())).unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    w.write_all(b"{\"List\":{\"query\":null}}\n").unwrap();
    w.flush().unwrap();
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    println!("{}", line.trim());
}
EOF
        rustc /tmp/_test_ipc2.rs -o "$KLIP_DIR/tmp/test_ipc2" 2>/dev/null || true
    fi

    # Start daemon with forced backend
    KLIP_WATCHER="$backend" RUST_LOG=info "$KLIP_DIR/target/debug/klipd" &
    DAEMON_PID=$!
    sleep 2

    # Test clipboard capture first
    echo "  → Clipboard capture:"
    echo "klip-test-$(date +%s)" | wl-copy 2>/dev/null || true
    sleep 2
    echo "  ✓ daemon should have captured it (check log above)"

    # Test IPC
    echo "  → IPC test:"
    if timeout 3 "$KLIP_DIR/tmp/test_ipc2" 2>/dev/null; then
        echo "  ✓ IPC works"
    else
        echo "  ✗ IPC failed (socket or daemon issue)"
    fi

    # Cleanup
    kill $DAEMON_PID 2>/dev/null || true
    wait $DAEMON_PID 2>/dev/null || true
    echo ""
    echo "=== Done testing $backend ==="
    echo ""
}

watch_logs() {
    echo "=== Watching daemon logs (Ctrl+C to stop) ==="
    pkill -x "klip" 2>/dev/null || true
    rm -f "$SOCKET"
    sleep 0.5
    RUST_LOG=debug "$KLIP_DIR/target/debug/klip"
}

# Main
cd "$KLIP_DIR"
build

case "${1:-all}" in
    kde)
        test_backend "kde"
        ;;
    gnome)
        test_backend "gnome"
        ;;
    x11)
        test_backend "x11"
        ;;
    watch)
        watch_logs
        ;;
    all)
        test_backend "kde"
        test_backend "gnome"
        test_backend "x11"
        echo "=== All backends tested ==="
        ;;
    *)
        echo "Usage: $0 [kde|gnome|x11|watch|all]"
        exit 1
        ;;
esac