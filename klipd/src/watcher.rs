use anyhow::Result;
use klip_common::ClipEntry;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;
use x11rb::protocol::xproto::ConnectionExt;

/// Supported clipboard backend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    Wayland,
    X11,
}

/// Detect which display server is running.
pub fn detect_backend() -> Backend {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        Backend::Wayland
    } else {
        Backend::X11
    }
}

/// Start watching the clipboard for changes.
/// Uses event-driven D-Bus on KDE, polling fallback on other desktops.
pub fn start_watcher(tx: Sender<ClipEntry>) -> Result<()> {
    let backend = detect_backend();
    log::info!("Starting clipboard watcher on {:?}", backend);

    match backend {
        Backend::Wayland => watch_wayland(tx),
        Backend::X11 => watch_x11(tx),
    }
}

// ── Wayland watcher ───────────────────────────────────────────────────────────

fn watch_wayland(tx: Sender<ClipEntry>) -> Result<()> {
    // Try KDE-specific D-Bus signal first (event-driven, zero CPU)
    // If that fails (not KDE, or D-Bus unavailable), fall back to polling
    if let Ok(()) = watch_kde_dbus(tx.clone()) {
        return Ok(());
    }
    log::info!("KDE D-Bus not available, falling back to wl-paste polling");
    watch_wayland_poll(tx)
}

/// Watch clipboard via KDE Klipper D-Bus signal `clipboardHistoryUpdated` on
/// `org.kde.klipper`. This is event-driven — zero CPU, instant notification.
/// Uses `dbus-monitor` as a subprocess (most reliable, no D-Bus library needed).
fn watch_kde_dbus(tx: Sender<ClipEntry>) -> Result<()> {
    log::info!("Trying KDE Klipper D-Bus clipboard monitoring...");

    // Check if Klipper is running
    let check = std::process::Command::new("dbus-send")
        .args([
            "--session", "--dest=org.freedesktop.DBus", "--print-reply",
            "/org/freedesktop/DBus", "org.freedesktop.DBus.NameHasOwner",
            "string:org.kde.klipper",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match check {
        Ok(s) if s.success() => {}
        _ => {
            log::debug!("KDE Klipper not running or dbus-send unavailable");
            return Err(anyhow::anyhow!("Klipper not available"));
        }
    }

    // Spawn dbus-monitor to listen for clipboardHistoryUpdated signals
    let mut child = std::process::Command::new("dbus-monitor")
        .args([
            "--session",
            "interface='org.kde.klipper.klipper',member='clipboardHistoryUpdated'",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("dbus-monitor failed: {}", e))?;

    let tx_for_thread = tx.clone();
    thread::spawn(move || {
        log::info!("KDE D-Bus clipboard monitoring active");
        let mut last_content: Option<String> = None;
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => return,
        };
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);

        // Read each line — when we see a signal line, clipboard changed
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            // clipboardHistoryUpdated signal received — read clipboard
            if line.contains("clipboardHistoryUpdated") || line.contains("member=") {
                if let Some(content) = read_clipboard_wl_paste() {
                    if Some(&content) != last_content.as_ref() {
                        last_content = Some(content.clone());
                        let entry = ClipEntry {
                            id: 0,
                            content,
                            mime_type: "text/plain".into(),
                            pinned: false,
                            created_at: String::new(),
                            updated_at: String::new(),
                        };
                        let _ = tx_for_thread.send(entry);
                    }
                }
            }
        }
    });

    Ok(())
}

/// Fallback polling watcher — only used when event-driven methods aren't available.
/// Polls every 5 seconds using `wl-paste --list-types` (instant, no data transfer)
/// and only reads full content when the available types change.
fn watch_wayland_poll(tx: Sender<ClipEntry>) -> Result<()> {
    let mut last_types: Option<String> = None;
    let mut last_content: Option<String> = None;

    loop {
        // Step 1: cheap check — list available MIME types (instant, no data)
        let current_types = get_clipboard_types();
        if current_types == last_types {
            thread::sleep(Duration::from_millis(5000));
            continue;
        }

        // Types changed — clipboard may have new content. Check if text is available.
        let has_text = current_types.as_deref().map_or(false, |t| {
            t.lines().any(|l| {
                l.starts_with("text/") || l == "UTF8_STRING" || l == "STRING" || l == "TEXT"
            })
        });
        last_types = current_types;

        if !has_text {
            last_content = None;
            thread::sleep(Duration::from_millis(5000));
            continue;
        }

        // Step 2: read the actual text content
        if let Some(content) = read_wl_paste_timeout(&["--no-newline"])
            .or_else(|| read_wl_paste_timeout(&[]))
        {
            if Some(&content) != last_content.as_ref() {
                last_content = Some(content.clone());
                let entry = ClipEntry {
                    id: 0,
                    content,
                    mime_type: "text/plain".into(),
                    pinned: false,
                    created_at: String::new(),
                    updated_at: String::new(),
                };
                let _ = tx.send(entry);
            }
        } else {
            last_content = None;
        }

        thread::sleep(Duration::from_millis(5000));
    }
}

/// Get the list of available clipboard MIME types (instant, no data transfer).
fn get_clipboard_types() -> Option<String> {
    let output = std::process::Command::new("wl-paste")
        .args(["--list-types"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        let types = String::from_utf8_lossy(&output.stdout).to_string();
        if types.is_empty() { None } else { Some(types) }
    } else {
        None
    }
}

/// Try to read clipboard via `wl-paste` command (standard Wayland protocol).
/// Uses a 1-second timeout to avoid hanging on image/non-text clipboard data.
fn read_clipboard_wl_paste() -> Option<String> {
    if !clipboard_has_text() {
        return None;
    }
    read_wl_paste_timeout(&["--no-newline"])
        .or_else(|| read_wl_paste_timeout(&[]))
}

/// Check if the clipboard currently contains text/plain content.
fn clipboard_has_text() -> bool {
    get_clipboard_types().map_or(false, |t| {
        t.lines().any(|l| {
            l.starts_with("text/") || l == "UTF8_STRING" || l == "STRING" || l == "TEXT"
        })
    })
}

/// Run wl-paste with a 1-second timeout. Returns None if it times out
/// (e.g. clipboard contains an image) or fails.
fn read_wl_paste_timeout(args: &[&str]) -> Option<String> {
    let mut child = std::process::Command::new("wl-paste")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(1);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    use std::io::Read;
                    let mut output = String::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = stdout.read_to_string(&mut output);
                    }
                    let trimmed = output.trim().to_string();
                    return if trimmed.is_empty() { None } else { Some(trimmed) };
                }
                return None;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Copy to clipboard via `wl-copy` command (standard Wayland protocol).
#[allow(dead_code)]
pub fn copy_to_clipboard_wl_copy(content: &str) -> Result<()> {
    let mut child = std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(content.as_bytes())?;
        drop(stdin);
    }
    child.wait()?;
    Ok(())
}

// ── X11 watcher ───────────────────────────────────────────────────────────────

fn watch_x11(tx: Sender<ClipEntry>) -> Result<()> {
    use x11rb::connection::Connection;
    use x11rb::rust_connection::RustConnection;

    let (conn, _screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[_screen_num];

    let clipboard_atom = conn
        .intern_atom(false, b"CLIPBOARD")?
        .reply()?
        .atom;
    let utf8_string_atom = conn
        .intern_atom(false, b"UTF8_STRING")?
        .reply()?
        .atom;

    let mut last_owner = 0;

    loop {
        let owner = conn
            .get_selection_owner(clipboard_atom)?
            .reply()?
            .owner;

        if owner != last_owner && owner != 0 {
            last_owner = owner;

            conn.convert_selection(
                screen.root,
                clipboard_atom,
                utf8_string_atom,
                clipboard_atom,
                0u32,
            )?;

            conn.flush()?;
            thread::sleep(Duration::from_millis(200));

            if let Ok(reply) = conn
                .get_property(false, screen.root, clipboard_atom, 0u32, 0u32, 1_000_000)?
                .reply()
            {
                if !reply.value.is_empty() {
                    let content = String::from_utf8_lossy(&reply.value).to_string();
                    if !content.is_empty() {
                        let entry = ClipEntry {
                            id: 0,
                            content,
                            mime_type: "text/plain".into(),
                            pinned: false,
                            created_at: String::new(),
                            updated_at: String::new(),
                        };
                        let _ = tx.send(entry);
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(500));
    }
}