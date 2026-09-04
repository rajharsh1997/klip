pub mod wayland_dc;
pub mod fallback;
pub mod x11;
pub mod kde;

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
/// - Wayland: `zwlr_data_control_v1` (KDE, GNOME ≥ 43, Sway, Hyprland) — event-driven, zero CPU
/// - Wayland fallback: `wl-paste` polling (GNOME < 43 / Ubuntu 22.04)
/// - X11: XFixes `SelectSelectionInput` — event-driven, zero CPU
///
/// Override via `KLIP_WATCHER=wayland|gnome|x11` env var for testing.
pub fn start_watcher(tx: Sender<ClipEntry>) -> Result<()> {
    // Allow env override for testing
    if let Ok(override_val) = std::env::var("KLIP_WATCHER") {
        match override_val.to_lowercase().as_str() {
            "wayland" | "dc" => {
                log::info!("KLIP_WATCHER=wayland forced — trying zwlr_data_control_v1");
                if wayland_dc::try_watch(tx).is_ok() {
                    return Ok(());
                }
                log::warn!("zwlr_data_control_v1 failed despite KLIP_WATCHER=wayland");
                return Err(anyhow::anyhow!("zwlr_data_control_v1 not available"));
            }
            "gnome" | "fallback" => {
                log::info!("KLIP_WATCHER={} forced — using polling fallback", override_val);
                return fallback::start_watch(tx);
            }
            "x11" => {
                log::info!("KLIP_WATCHER=x11 forced");
                return x11::start_watch(tx);
            }
            other => {
                log::warn!("Unknown KLIP_WATCHER={}, falling back to auto-detect", other);
            }
        }
    }

    let backend = detect_backend();
    log::info!("Starting clipboard watcher on {:?}", backend);

    match backend {
        Backend::Wayland => {
            // Try KDE Klipper D-Bus first (best support for KDE Plasma Wayland)
            if kde::try_watch(tx.clone()).is_ok() {
                return Ok(());
            }
            // Try zwlr_data_control_v1 next — works on GNOME 43+, Sway, Hyprland
            if wayland_dc::try_watch(tx.clone()).is_ok() {
                return Ok(());
            }
            // Fallback 1: Try X11 (XWayland) which bypasses GNOME's strict Wayland restrictions.
            // Mutter automatically syncs the Wayland clipboard to the X11 clipboard, allowing
            // XFixes to capture it perfectly in the background with zero CPU!
            if x11::start_watch(tx.clone()).is_ok() {
                log::info!("zwlr_data_control_v1 not available, but XWayland fallback succeeded");
                return Ok(());
            }

            // Fallback 2: polling (only if XWayland is completely disabled)
            log::info!("zwlr_data_control_v1 and X11 not available, using polling fallback");
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

/// Build a ClipEntry from text content with automatic type detection.
pub fn make_entry(content: String) -> ClipEntry {
    let mime_type = detect_content_type(&content);
    ClipEntry {
        id: 0,
        content,
        mime_type,
        pinned: false,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

/// Detect the semantic type of clipboard content.
///
/// Returns a mime-type-like string used by the GUI to show icons:
///   text/uri-list  → URL
///   text/x-email   → email address
///   text/x-path    → file system path
///   text/x-color   → hex color code
///   text/x-code    → code snippet
///   text/plain     → everything else
pub fn detect_content_type(content: &str) -> String {
    let t = content.trim();

    // URL
    if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("ftp://") {
        return "text/uri-list".into();
    }

    // Email: single token containing @ with a dot in the domain
    if !t.contains(' ') && !t.contains('\n') {
        if let Some(pos) = t.find('@') {
            let after = &t[pos + 1..];
            if !after.is_empty() && after.contains('.') && !after.starts_with('.') {
                return "text/x-email".into();
            }
        }
    }

    // Hex color: #RGB, #RRGGBB, #RRGGBBAA
    if !t.contains(' ') && !t.contains('\n') && t.starts_with('#') {
        let hex = &t[1..];
        if matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return "text/x-color".into();
        }
    }

    // File path: starts with / or ~/
    if !t.contains('\n') && (t.starts_with('/') || t.starts_with("~/")) {
        return "text/x-path".into();
    }

    // Code: indented multi-line or common code tokens
    let has_indent = t.lines().skip(1).any(|l| l.starts_with("    ") || l.starts_with('\t'));
    let has_code = ["() {", "fn ", "def ", "class ", "import ", "const ", "let ",
                    " => ", "};", "return ", "if (", "for ("]
        .iter().any(|tok| t.contains(tok));
    if has_indent || has_code {
        return "text/x-code".into();
    }

    "text/plain".into()
}