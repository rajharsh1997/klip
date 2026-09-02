pub mod kde;
pub mod fallback;
pub mod x11;

use anyhow::Result;
use klip_common::ClipEntry;
use std::sync::mpsc::Sender;

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
/// Dispatches to the appropriate backend:
/// - KDE: event-driven D-Bus signals (zero CPU)
/// - GNOME/other Wayland: polling via wl-paste --list-types
/// - X11: selection owner tracking via x11rb
pub fn start_watcher(tx: Sender<ClipEntry>) -> Result<()> {
    let backend = detect_backend();
    log::info!("Starting clipboard watcher on {:?}", backend);

    match backend {
        Backend::Wayland => {
            // Try KDE D-Bus first (event-driven, zero CPU)
            if kde::try_watch(tx.clone()).is_ok() {
                return Ok(());
            }
            log::info!("KDE D-Bus not available, using polling fallback");
            fallback::start_watch(tx)
        }
        Backend::X11 => x11::start_watch(tx),
    }
}

// ── Shared utilities (used by all backends) ──────────────────────────────────

/// Get the list of available clipboard MIME types (instant, no data transfer).
pub fn get_clipboard_types() -> Option<String> {
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

/// Check if the clipboard currently contains text/plain content.
pub fn clipboard_has_text() -> bool {
    get_clipboard_types().map_or(false, |t| {
        t.lines().any(|l| {
            l.starts_with("text/") || l == "UTF8_STRING" || l == "STRING" || l == "TEXT"
        })
    })
}

/// Try to read clipboard via `wl-paste` command (standard Wayland protocol).
/// Uses a 1-second timeout to avoid hanging on image/non-text clipboard data.
pub fn read_clipboard_wl_paste() -> Option<String> {
    if !clipboard_has_text() {
        return None;
    }
    read_wl_paste_timeout(&["--no-newline"])
        .or_else(|| read_wl_paste_timeout(&[]))
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
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Build a ClipEntry from text content (used by all backends).
pub fn make_entry(content: String) -> ClipEntry {
    ClipEntry {
        id: 0,
        content,
        mime_type: "text/plain".into(),
        pinned: false,
        created_at: String::new(),
        updated_at: String::new(),
    }
}