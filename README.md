# Klip — Native Linux Clipboard Manager

A keyboard-driven clipboard manager for Linux, inspired by Maccy but built natively for the Linux ecosystem.

## Features

- **Lightweight daemon** — <10 MB RAM idle, microsecond response times
- **Floating palette UI** — summoned via global shortcut, auto-dismisses on focus loss
- **Instant search** — fuzzy search through clipboard history
- **Quick-copy badges** — press `1`–`9` to instantly paste any of the top 9 entries
- **Pin important clips** — keep frequently used snippets pinned
- **Wayland & X11 support** — works on both display servers
- **SQLite-backed** — persistent history with WAL mode for performance

## Architecture

```
┌─────────────┐     Unix Socket      ┌──────────────────────────────┐
│   klip       │ ◄──────────────────► │       klipd            │
│  (GTK4 GUI)  │     JSON/IPC         │       (Daemon)          │
└─────────────┘                      │                              │
                                     │  ┌────────────────────────┐  │
                                     │  │    Clipboard Watcher   │  │
                                     │  │  ┌──────┐┌──────┐┌──┐ │  │
                                     │  │  │ KDE  ││GNOME ││X11│ │  │
                                     │  │  │ D-Bus││Poll  ││   │ │  │
                                     │  │  └──────┘└──────┘└──┘ │  │
                                     │  └──────────┬─────────────┘  │
                                     │             ▼                │
                                     │  ┌────────────────────────┐  │
                                     │  │    Common History      │  │
                                     │  │  ┌──────┐┌──────┐┌──┐ │  │
                                     │  │  │SQLite││Search││...│ │  │
                                     │  │  └──────┘└──────┘└──┘ │  │
                                     │  └────────────────────────┘  │
                                     └──────────────────────────────┘
```

## Clipboard Watcher Backends

Klip supports multiple clipboard monitoring strategies, automatically selecting the best one for your desktop:

| Backend | Desktop | Method | CPU Usage | Latency |
|---------|---------|--------|-----------|---------|
| **KDE D-Bus** | KDE Plasma | Listens for Klipper `clipboardHistoryUpdated` signal via `dbus-monitor` | Zero (event-driven) | Instant |
| **GNOME/Other** | GNOME, Sway, wlroots | Polls `wl-paste --list-types` every 5s for MIME type changes, reads text only when types change | Minimal (5s interval, no data transfer) | ~5s |
| **X11** | X11/XWayland | Tracks clipboard selection owner changes via `x11rb` | Event-driven | Instant |

### Testing Backends

You can force a specific backend using the `KLIP_WATCHER` environment variable:

```bash
# Test KDE backend (requires Klipper running)
KLIP_WATCHER=kde klipd

# Test GNOME/fallback polling backend
KLIP_WATCHER=gnome klipd

# Test X11 backend
KLIP_WATCHER=x11 klipd
```

Or use the test script:

```bash
# Test all backends
./test-backend.sh all

# Test a specific backend
./test-backend.sh kde
./test-backend.sh gnome
./test-backend.sh x11

# Watch daemon logs with debug output
./test-backend.sh watch
```

## Quick Start

### Build from source

```bash
# Build
./build.sh

# Install (system-wide, requires sudo)
./install.sh

# Or install to ~/.local/bin without sudo
BIN_DIR="$HOME/.local/bin" ./install.sh
```

### Launch the GUI (daemon auto-starts)

```bash
klip
```

That's it! `klip` automatically starts the daemon if it's not already running — no need to run `systemctl` manually. The daemon will keep running in the background for future invocations.

> **Note**: If you prefer the daemon to start automatically on login, you can still enable it:
> ```bash
> systemctl --user enable klipd
> ```

## Install from Packages

Pre-built packages are available on the [Releases page](https://github.com/rajharsh1997/klip/releases).

### Debian / Ubuntu (.deb)

```bash
sudo dpkg -i klip_*.deb
```

### Fedora / RHEL (.rpm)

```bash
sudo dnf install ./klip-*.rpm
```

After installing, launch `klip` from your app tray or terminal — the daemon auto-starts.

The package includes everything: `klip` (GUI), `klipd` (daemon), app icon, `.desktop` entry, and a systemd user service.

## Installation Options

### Option 1: Install from package (recommended)

Download the `.deb` or `.rpm` from the [Releases page](https://github.com/rajharsh1997/klip/releases) and install it. One package includes everything: both binaries, icons, `.desktop` entry, and systemd service.

### Option 2: Build & install from source

```bash
git clone https://github.com/rajharsh1997/klip.git
cd klip
./install.sh
```

This builds and installs the binaries, icons, `.desktop` file, and systemd user service. The daemon starts automatically when you launch `klip`.

### Option 3: Manual install

```bash
cargo build --release
sudo install target/release/klipd target/release/klip /usr/local/bin/
```

### Option 3: User-only install (no sudo)

```bash
cargo build --release
mkdir -p ~/.local/bin
install target/release/klipd ~/.local/bin/
install target/release/klip ~/.local/bin/
# Add ~/.local/bin to your PATH if not already
```

### Uninstall

```bash
rm -f $(which klip) $(which klipd)
rm -f $HOME/.local/share/applications/klip.desktop
rm -f $HOME/.config/systemd/user/klip.service
rm -f $HOME/.local/share/icons/hicolor/*/apps/klip.png
sudo rm -f /usr/local/share/icons/hicolor/*/apps/klip.png 2>/dev/null
systemctl --user daemon-reload
```

## Global Shortcut

Bind a global shortcut (e.g., `Ctrl+Alt+V`) to `klip` in your desktop environment's keyboard settings.

## Keyboard Shortcuts (within GUI)

| Key | Action |
|-----|--------|
| `1`–`9` | Copy entry at that position |
| `Escape` | Close palette |
| `Ctrl+Backspace` | Clear unpinned history |
| Type to search | Fuzzy filter entries |

## Project Structure

```
klip/
├── klip-common/     # Shared types & IPC protocol
├── klip/           # Background daemon (produces klipd binary)
│   ├── src/
│   │   ├── main.rs          # Entry point
│   │   ├── storage.rs       # SQLite storage engine
│   │   ├── watcher/         # Clipboard watcher (multi-backend)
│   │   │   ├── mod.rs       # Dispatcher & shared utilities
│   │   │   ├── kde.rs       # KDE D-Bus event-driven backend
│   │   │   ├── fallback.rs  # GNOME/other polling backend
│   │   │   └── x11.rs       # X11 selection tracking backend
│   │   └── ipc.rs           # Unix socket IPC server
├── klip-gui/        # GTK4 floating palette (produces klip binary)
│   ├── src/
│   │   ├── main.rs      # GTK4 UI
│   │   ├── client.rs    # IPC client
│   │   └── style.css    # Styling
├── build.sh         # Build script
├── icons/           # App icons (48x48, 128x128, 256x256)
├── install.sh       # Install script
├── klip.desktop     # Desktop entry for app tray / shortcut binding
├── klip.service     # systemd user service (starts klipd)
├── test-backend.sh  # Backend test script
```

## License

MIT