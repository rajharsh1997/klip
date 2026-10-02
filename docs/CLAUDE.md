# Klip — Codebase Notes

Keyboard-driven clipboard manager for Linux (Maccy-like). Rust workspace, two binaries:
- **`klipd`**: background daemon. Watches the clipboard, stores history in SQLite, and serves a Unix-socket IPC.
- **`klip`**: GTK4 floating palette plus a system-tray icon (ksni). Talks to the daemon over IPC.

It ships as `.deb` (cargo-deb) and `.rpm` (cargo-generate-rpm) through GitHub Actions on `v*` tags.

## Workspace layout (naming is confusing; read carefully)

| Crate dir | Package name | Produces binary | Role |
|---|---|---|---|
| `klip-common/` | `klip-common` | (lib) | Shared types: `ClipEntry`, `DaemonRequest`, `DaemonResponse`, `DaemonEvent` |
| `klip/` | `klip` | **`klipd`** | Daemon. Also holds **all packaging metadata** (`[package.metadata.deb]` / `generate-rpm`) |
| `klip-gui/` | `klip-gui` | **`klip`** | GTK4 GUI and tray |
| `klipd/` | — | — | **Stale leftover, not in the workspace.** An old copy of the daemon with swapped bin names and no `watcher/` dir. Ignore or delete. |

Workspace members (root `Cargo.toml`): `klip-common`, `klip`, `klip-gui`. Version `0.1.0`, edition 2021.

Other files:
- `klip-gui/src/bin/test_tray.rs`: cargo auto-discovers this, so it **builds as an extra `test_tray` binary**.
- `klip-gui/src/bin.rs`: empty `fn main(){}`, unused.
- Root `test_glib.rs`, `test_ksni.rs`, `test_ksni_ast.rs`, `test_tray.rs`: scratch files, not built.
- `gui-visibility-issue.md`: old debugging notes (the window didn't show on GNOME Wayland). Now resolved via `Application` + `hold()`.
- `test-backend.sh`, `test-stale-socket.sh`: manual test scripts. They contain hard-coded paths and pkill the wrong process names in places.
- `maintainer-scripts/postinst`: deb-only cleanup of old binary, service, and desktop names from pre-rename versions.

## Daemon (`klip/src/`)

**`main.rs`**
- Data dir: `$XDG_DATA_HOME/klip`, falling back to `~/.local/share/klip`. It holds `klip.db` and `klip.sock`.
- Threads:
  - watcher thread → `mpsc<ClipEntry>`
  - storage processor (insert, then emit `DaemonEvent`)
  - IPC accept loop on the main thread, one thread per client
- It unconditionally `remove_file`s the socket before `bind`. A second daemon instance will steal the socket.

**`storage.rs`** (rusqlite, bundled SQLite, WAL)
- Table: `clips(id, content, mime_type, pinned, created_at, updated_at)`.
- `insert` dedups on `(content, mime_type)`. A duplicate only bumps `updated_at`.
- `list(None)`: `ORDER BY pinned DESC, updated_at DESC LIMIT 500`.
- `list(Some(q))`: `LIKE` search, no limit. The GUI doesn't use it.
- One `Mutex<Connection>`. Methods drop the lock before calling `get_by_id`, because the mutex isn't reentrant.
- No history-size cap or pruning. The DB grows forever.

**`ipc.rs`**
- Protocol: newline-delimited JSON, using serde's default externally-tagged enums:
  - `{"List":{"query":null}}`
  - `"Count"`
  - `{"Copy":{"id":5}}`
  - `{"TogglePin":{"id":5}}`
  - `{"Delete":{"id":5}}`
  - `"ClearHistory"`
- Responses: `{"Entries":[...]}`, `{"Count":n}`, `"Ok"`, `{"Error":"..."}`.
- `Copy`:
  - Wayland: tries `wl-clipboard-rs` (wlr-data-control) first, then falls back to the `wl-copy` subprocess.
  - X11: uses the `xclip` subprocess. `xclip` isn't a declared package dependency, and the child is never waited on.
- `DaemonEvent`s are drained and discarded on every request. No push to clients is implemented.

**`watcher/`** (backend selection is in `mod.rs::start_watcher`)
- `KLIP_WATCHER=wayland|dc|gnome|fallback|x11` forces a backend.
- Auto-detect: `WAYLAND_DISPLAY` set → Wayland, otherwise X11.
- Wayland order:
  1. `kde.rs`: checks `org.kde.klipper` via `dbus-send NameHasOwner`, spawns `dbus-monitor`, and on each signal line reads the clipboard via `wl-paste`. Its match is `contains("member=")`, which fires on *any* signal line, including the initial `NameAcquired`.
  2. `wayland_dc.rs`: native `zwlr_data_control_v1` client (wayland-client 0.31). Reads offers through a pipe.
  3. `x11.rs` via XWayland.
  4. `fallback.rs`: `wl-paste --list-types` poll every 5 s.
- X11: `x11.rs` uses XFixes `SelectSelectionInput` (event-driven). There's a polling fallback if XFixes is missing.
- Shared helpers:
  - `read_clipboard_wl_paste`: 1 s timeout. **It `trim()`s the content**, so leading and trailing whitespace is lost.
  - `make_entry` and `detect_content_type`, a heuristic `mime_type`. Values:
    - `text/uri-list`
    - `text/x-email`
    - `text/x-color`
    - `text/x-path`
    - `text/x-code`
    - `text/plain`
- `x11.rs` builds `ClipEntry` by hand with `mime_type: "text/plain"`. It skips `make_entry`, so there's no type detection on X11.

## GUI (`klip-gui/src/main.rs`, `client.rs`, `style.css`)

- `gtk4::Application` with id `com.klip.clipboard-manager`. Single instance, so a second `klip` invocation just activates the first. `hold()` keeps the process alive with zero windows; the tray lives on.
- **Activate:**
  - `ensure_daemon_running` connects to the socket. If that fails, it tries `systemctl --user start klipd` and polls for the socket for 2 s.
  - Only if `systemctl` itself *fails* does it spawn `klipd` directly.
  - This all runs on the GTK main thread.
- **Window:** `build_ui`
  - Undecorated 320×480 `ApplicationWindow`.
  - Closes on focus loss (150 ms debounce) and on Esc.
  - `close()` destroys the window. The next activate rebuilds it and refetches entries.
- **Data and search:**
  - Fetches `List{query:None}` once, so at most 500 entries.
  - Fuzzy filtering happens client-side with `nucleo`, debounced 150 ms.
  - Shows the top 50, pinned first, with "Pinned" and "History" section headers.
- **Keys:**
  - `1`–`9` quick-copy, only when the search box is empty.
  - `Esc` closes.
  - `Ctrl+Backspace` clears unpinned history, but the list isn't refreshed afterwards.
  - Row activation copies and closes.
- **No UI for pin or delete**, even though the IPC and `client.rs` functions exist (`#[allow(dead_code)]`).
- **Tray:** ksni (StatusNotifierItem) with a "Toggle Window" / "Quit" menu. `tray.spawn().unwrap()`.
- **CSS:** dark glass theme. Its `@import` of Google Fonts fails ("Failed to import: Operation not supported" warning) because GTK can't import remote URLs.
- **Row text:** truncation uses byte slicing (`&content[..120]`, tooltip `&full_content[..1000]`).

## Packaging and install

- Packaging metadata lives in `klip/Cargo.toml`. Both `cargo deb -p klip` and `cargo generate-rpm -p klip` bundle:
  - both binaries
  - icons (48, 128, 256)
  - `klip.desktop`
  - `klipd.service` → `/usr/lib/systemd/user/`
- The deb depends on `$auto, wl-clipboard`. The **RPM declares no `requires`** (no wl-clipboard).
- CI (`.github/workflows/package.yml`): ubuntu-24.04 builds the deb; a fedora:43 container builds the rpm. On a tag, it creates a draft GitHub release. The files are renamed with the tag version, but **the package metadata stays `0.1.0`** (installed RPM reports `klip-0.1.0-1`).
- `install.sh` uses the system-wide path whenever `sudo` exists. It copies `target/release/klip-gui`, **which doesn't exist** (the GUI bin is `klip`).
- `klipd.service` has `ExecStart=/usr/local/bin/klipd` hard-coded. It also has `PassEnvironment=DISPLAY WAYLAND_DISPLAY ...`.

## Dev environment (this machine)

- Fedora 44, **KDE Plasma on Wayland** (`WAYLAND_DISPLAY=wayland-0`, `DISPLAY=:0`).
  - KWin offers `ext_data_control_v1` but not `zwlr_data_control_v1`.
  - Klipper is running.
- Installed: RPM `klip-0.1.0-1` (old build, `/usr/bin/klip{,d}`), `wl-clipboard` 2.2.1 (uses ext-data-control), `spectacle`.
- **Build inside the `klip-dev` toolbox.** The host has no gcc/rust/-devel packages, and sudo needs a password. The toolbox is Fedora 44 with gcc, rust, gtk4-devel, libadwaita-devel and libxcb-devel, so its binaries run natively on the host.
  ```bash
  toolbox run -c klip-dev cargo build --release
  ```
- Test the daemon:
  ```bash
  pkill -x klipd; RUST_LOG=debug ./target/release/klipd
  ```
  Then `wl-copy` some text and query over IPC:
  ```bash
  python3 -c 'import socket,os;s=socket.socket(socket.AF_UNIX);s.connect(os.path.expanduser("~/.local/share/klip/klip.sock"));s.sendall(b"\"Count\"\n");print(s.recv(99999))'
  ```
- Force a backend: `KLIP_WATCHER=wayland|kde|x11|gnome`.
- **GUI input can't be automated here.** XTEST injection makes KDE pop a "Remote Control" permission prompt, and there's no ydotool or wtype. Ask the user to test keyboard and click behaviour by hand.
- Count tray registrations:
  ```bash
  dbus-monitor --session "type='method_call',member='RegisterStatusNotifierItem'"
  ```
- Screenshots: `spectacle -b -n -f -o out.png`.

## Fixed (2026-10-02, uncommitted)

Each fix was verified on KDE Wayland.

- **Wayland capture, `wayland_dc.rs` rewrite.** Now supports `ext_data_control_v1` (preferred) and `zwlr_data_control_v1`. Bugs fixed:
  - Missing `event_created_child!` meant a panic on the first DataOffer.
  - `receive` wasn't flushed before reading the pipe (hang).
  - Primary-selection offers leaked.
  - There was no read timeout.

  Result: 40/40 rapid copies captured, 300 KB clip OK.
- **Backend order** (`watcher/mod.rs`): data-control → KDE Klipper D-Bus → XWayland XFixes (logs a warning) → polling.
- **Deps:** `wayland-protocols 0.32` (staging), `wayland-protocols-wlr 0.3`, `wl-clipboard-rs 0.9`. The last gives native copy-back on KDE 6 with no `wl-copy` fallback.
- **wl-paste reader:** stdout is drained on a thread, so clips over 64 KB are no longer dropped. Content is no longer `trim()`med.
- **KDE watcher:** now matches only `member=clipboardHistoryUpdated`.
- **X11 watcher:** uses `make_entry`, so it gets type detection.
- **GUI quick-copy 1–9:** the key controller now runs in the **capture** phase; before, the focused SearchEntry ate the digits. Also added Alt+1–9 (works while searching) and keypad digits.
- **GUI tray:** spawned in `connect_startup`, so it only runs in the primary instance. Before, every hotkey launch registered a temporary duplicate tray icon (verified via dbus-monitor: 1 → 0). `spawn()` errors no longer panic.
- **GUI:** char-safe truncation (`truncate_chars`) replaces the byte slicing that panicked on non-ASCII. Ctrl+Backspace now refreshes the list. The remote CSS `@import` was removed.
- **GUI `ensure_daemon_running`:** verifies the socket actually connects. If it doesn't after `systemctl start`, it stops the unit and spawns `klipd` directly.
- **Daemon:** exits if another instance already serves the socket.
- **`klipd.service`:** `ExecStart=klipd`. systemd resolves bare names via `/usr/local/bin`, `/usr/bin`, ….
- **`install.sh`:** installs `target/release/klip` (was the non-existent `klip-gui`).

## Ubuntu/GNOME missed copies: reproduced and fixed (simulated)

**Test rig:** an `klip-ubuntu` toolbox (Ubuntu 24.04, **mutter 46.2**) running
```bash
dbus-run-session -- mutter --headless --wayland --virtual-monitor 1280x800 --wayland-display klip-gnome
```
- Use a private D-Bus, so the host's Klipper isn't detected.
- Headless mutter has no keyboard, so Wayland clients can't set the clipboard. Hold a `org.gnome.Mutter.RemoteDesktop` session open (CreateSession + Start, kept alive by a Python Gio process) and tap a key to add a virtual keyboard.
- Run klipd on the host with:
  - `WAYLAND_DISPLAY=klip-gnome`
  - `DISPLAY=:1`
  - `XAUTHORITY=$XDG_RUNTIME_DIR/.mutter-Xwaylandauth.*`
  - a short `XDG_DATA_HOME`, because the socket path must be under 108 chars
- Careful: `pkill -f <pattern>` in a Bash call can match the calling shell itself (exit 144). Kill by PID.

**Findings:**
- GNOME 46 has no data-control → klipd uses XWayland XFixes. **Mutter mirrors every copy to X11 eagerly: 10/10 captured.** (KWin does not.)
- When `XAUTHORITY` is missing, the X11 connect fails and klipd silently drops to 5 s polling.
  - This happens as a systemd service: `klipd.service` didn't pass it.
  - Result: **4/10 captured with the old build**. This is the user's "sometimes only after 5–6 copies".

**Fix:**
- `watcher::ensure_xwayland_auth()` defaults `DISPLAY=:0` and discovers the Xwayland auth file in `$XDG_RUNTIME_DIR` (`.mutter-Xwaylandauth.*`, `xauth_*`).
- `klipd.service` now passes `XAUTHORITY`.
- Result: 20/20 without `XAUTHORITY`.

**Not yet verified on a real Ubuntu machine.** To check there:
```bash
journalctl --user -u klipd | grep -i watcher
```
It should say "Using XWayland clipboard monitoring", not "polling".

**Remaining risk:** polling is still lossy if it ever gets chosen. Consider retrying better backends periodically, or a GNOME Shell extension.

## Known bugs and issues (original audit; see "Fixed" above for what's done)

### Confirmed

1. **`klipd.service` `ExecStart=/usr/local/bin/klipd`**, but the packages install to `/usr/bin`.
   - Effect: the service crash-loops forever (restart counter was at 2762).
   - Worse, `systemctl --user start` still returns success. So `ensure_daemon_running` never falls back to spawning `klipd` directly, and the GUI shows an empty list.
2. `install.sh` installs `target/release/klip-gui`, which doesn't exist (should be `target/release/klip`).
3. Package version is stuck at `0.1.0`. Tags aren't propagated into the deb/rpm metadata, so `dnf`/`apt` won't treat new releases as upgrades.
4. GUI byte-slices UTF-8 (`&content[..120]`, `&full_content[..1000]`). It **panics on multi-byte chars** (emoji, Hindi, etc.) at those boundaries.
5. Ctrl+Backspace clears history, but the list isn't refreshed.
6. CSS `@import` of a remote Google Font is unsupported in GTK and produces a warning.
7. The RPM lacks a `wl-clipboard` dependency. X11 copy needs `xclip`, which neither package depends on.
8. `klip-gui/src/bin/test_tray.rs` builds and ships as a stray binary target. There's also the dead `klipd/` dir and root scratch `test_*.rs` files.
9. Content is `trim()`med in the wl-paste paths (whitespace lost); `wayland_dc` trims trailing `\n`s.
10. `x11.rs` skips `detect_content_type`, so everything is `text/plain` on X11.
11. Daemon startup deletes any existing socket without checking for a live daemon, so two daemons can run.

### Suspected (verify before fixing)

- `wayland_dc::read_text_from_offer` calls `offer.receive()` and then blocks reading the pipe **without flushing the Wayland connection**. The fd sits in the send buffer, so the read likely hangs the watcher thread forever. It needs a `conn.flush()` before reading, plus a timeout. Untestable on this KDE (no wlr protocol); test on Sway/Hyprland/GNOME.
- `tray.spawn().unwrap()`: ksni 0.3 may return an error when no StatusNotifierWatcher exists (stock GNOME without the AppIndicator extension), which would panic the GUI at startup.
- `wayland_dc`: offers from `PrimarySelection`, and offers that are never selected, are never destroyed or removed from `pending_mimes`. A small leak.
- KDE watcher: if `dbus-monitor` dies, the thread exits silently and there's no fallback.
- The app id `com.klip.clipboard-manager` doesn't match `klip.desktop`. On Wayland the window may get a generic icon. It needs `StartupWMClass` or a renamed desktop file.
- Search only covers the 500 most recent entries (client-side fuzzy over `List(None)`).

### Feature gaps

- No pin/delete UI (backend exists).
- No history size limit.
- No image support.
- No paste-into-app (copy only).
- No settings or config file.
- `DaemonEvent` push is unused.
- No `ext_data_control_v1` backend for modern KDE and wlroots.
