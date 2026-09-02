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
┌─────────────┐     Unix Socket      ┌──────────────┐
│   klip-gui  │ ◄──────────────────► │    klipd     │
│  (GTK4 GUI) │     JSON/IPC         │  (Daemon)    │
└─────────────┘                      │              │
                                     │  ┌────────┐  │
                                     │  │ SQLite │  │
                                     │  └────────┘  │
                                     │  ┌──────────┐ │
                                     │  │ Watcher  │ │
                                     │  │(Wl/X11)  │ │
                                     │  └──────────┘ │
                                     └──────────────┘
```

## Quick Start

```bash
# Build
./build.sh

# Install
./install.sh

# Start the daemon
systemctl --user start klipd
systemctl --user enable klipd

# Launch the GUI
klip-gui
```

## Global Shortcut

Bind a global shortcut (e.g., `Ctrl+Alt+V`) to `klip-gui` in your desktop environment's keyboard settings.

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
├── klipd/           # Background daemon
│   ├── src/
│   │   ├── main.rs      # Entry point
│   │   ├── storage.rs   # SQLite storage engine
│   │   ├── watcher.rs   # Clipboard listener (Wayland/X11)
│   │   └── ipc.rs       # Unix socket IPC server
├── klip-gui/        # GTK4 floating palette
│   ├── src/
│   │   ├── main.rs      # GTK4 UI
│   │   ├── client.rs    # IPC client
│   │   └── style.css    # Styling
├── build.sh         # Build script
├── install.sh       # Install script
└── klipd.service    # systemd user service
```

## License

MIT