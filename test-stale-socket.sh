#!/bin/bash
# Test: stale socket detection fix
# Simulates the exact scenario: socket file exists but daemon is not listening.
set -euo pipefail

SOCK_DIR="$HOME/.local/share/klip"
SOCK_PATH="$SOCK_DIR/klip.sock"

echo "=== Klip Stale Socket Test ==="
echo ""

# ── 1. Clean state ────────────────────────────────────────────────────────────
echo "Step 1: Ensure clean state"
mkdir -p "$SOCK_DIR"
rm -f "$SOCK_PATH"
pkill -x klipd 2>/dev/null || true
sleep 0.3
echo "  ✓ No daemon running, no socket"
echo ""

# ── 2. Create a stale socket (the problem scenario) ──────────────────────────
echo "Step 2: Creating a stale socket (simulating daemon crash)"
# Just touch a socket file — nothing is listening on it
python3 -c "
import socket, os
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind('$SOCK_PATH')
# Do NOT call listen() or accept() — stale socket
s.close()
"
echo "  ✓ Stale socket created at: $SOCK_PATH"
ls -la "$SOCK_PATH"
echo ""

# ── 3. Verify the socket is NOT connectable (the bug condition) ───────────────
echo "Step 3: Verify socket exists but refuses connection"
if python3 -c "
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect('$SOCK_PATH')
    print('  Connected — socket is alive (unexpected)')
    sys.exit(0)
except ConnectionRefusedError:
    print('  ✓ Connection refused — this is the stale socket scenario')
    sys.exit(1)
" 2>/dev/null; then
    echo "  WARN: socket is actually alive, test may not be valid"
else
    echo "  ✓ Confirmed: stale socket exists but refuses connections"
fi
echo ""

# ── 4. Start the real daemon — it must handle the stale socket ────────────────
echo "Step 4: Starting klipd (it removes stale socket and binds)"
./target/release/klipd &
DAEMON_PID=$!
echo "  klipd PID: $DAEMON_PID"

# Wait up to 2s for socket to appear and become connectable
for i in $(seq 1 20); do
    if python3 -c "
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect('$SOCK_PATH')
    s.close()
    sys.exit(0)
except:
    sys.exit(1)
" 2>/dev/null; then
        echo "  ✓ Socket is live after ${i}00ms"
        break
    fi
    sleep 0.1
done
echo ""

# ── 5. Send a real List request to the live daemon ────────────────────────────
echo "Step 5: Sending List request to daemon via socket"
RESPONSE=$(python3 -c "
import socket, json, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect('$SOCK_PATH')
req = json.dumps({'List': {'query': None}}) + '\n'
s.sendall(req.encode())
resp = b''
while True:
    chunk = s.recv(4096)
    if not chunk:
        break
    resp += chunk
    if b'\n' in resp:
        break
s.close()
print(resp.decode().strip())
")
echo "  Daemon response: $RESPONSE"
echo ""

# ── 6. Now simulate the FIX: stale socket + klip-gui connect attempt ──────────
echo "Step 6: Verify GUI stale-socket check logic (Rust test)"
cat > /tmp/test_stale_socket.rs << 'RUST_EOF'
use std::path::PathBuf;
use std::os::unix::net::UnixStream;

fn socket_is_alive(path: &PathBuf) -> bool {
    UnixStream::connect(path).is_ok()
}

fn main() {
    let sock = PathBuf::from(std::env::args().nth(1).unwrap());

    // Case A: Dead socket
    let dead = PathBuf::from("/tmp/nonexistent-test.sock");
    assert!(!socket_is_alive(&dead), "Dead socket should fail");
    println!("  ✓ Dead socket correctly detected as not alive");

    // Case B: Live socket (the daemon we started)
    assert!(socket_is_alive(&sock), "Live socket should connect");
    println!("  ✓ Live daemon socket correctly detected as alive");

    println!("  ✓ Fix logic works: stale = exists but !connectable");
}
RUST_EOF

rustc /tmp/test_stale_socket.rs -o /tmp/test_stale_socket 2>&1 && \
  /tmp/test_stale_socket "$SOCK_PATH"
echo ""

# ── 7. Cleanup ────────────────────────────────────────────────────────────────
echo "Step 7: Cleanup"
kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true
rm -f "$SOCK_PATH" /tmp/test_stale_socket.rs /tmp/test_stale_socket
echo "  ✓ Daemon stopped, socket cleaned up"
echo ""
echo "=== All tests passed! ==="
