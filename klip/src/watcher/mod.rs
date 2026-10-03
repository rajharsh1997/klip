pub mod wayland_dc;
pub mod fallback;
pub mod x11;
pub mod kde;

use anyhow::Result;
use klip_common::Config;
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
/// - Wayland: `ext_data_control_v1` / `zwlr_data_control_v1` (KDE, Sway, Hyprland, …) — event-driven, zero CPU
/// - Wayland, no data-control: KDE Klipper D-Bus, then XWayland XFixes, then `wl-paste` polling
/// - X11: XFixes `SelectSelectionInput` — event-driven, zero CPU
///
/// Override via `KLIP_WATCHER=wayland|kde|gnome|x11` env var for testing.
pub fn start_watcher(tx: Sender<Clip>, config: &Config) -> Result<()> {
    // Allow env override for testing
    if let Ok(override_val) = std::env::var("KLIP_WATCHER") {
        match override_val.to_lowercase().as_str() {
            "wayland" | "dc" => {
                log::info!("KLIP_WATCHER=wayland forced — trying data-control");
                return wayland_dc::try_watch(tx, config);
            }
            "kde" => {
                log::info!("KLIP_WATCHER=kde forced");
                return kde::try_watch(tx, config);
            }
            "gnome" | "fallback" => {
                log::info!("KLIP_WATCHER={} forced — using polling fallback", override_val);
                return fallback::start_watch(tx, config);
            }
            "x11" => {
                log::info!("KLIP_WATCHER=x11 forced");
                return x11::start_watch(tx, config);
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
            // Native data-control protocol — the most reliable option where available
            match wayland_dc::try_watch(tx.clone(), config) {
                Ok(()) => return Ok(()),
                Err(e) => log::info!("Wayland data-control unavailable: {e}"),
            }
            // KDE Klipper D-Bus (older Plasma without data-control)
            if kde::try_watch(tx.clone(), config).is_ok() {
                return Ok(());
            }
            // XWayland XFixes — the GNOME path (Mutter has no data-control).
            // Mutter mirrors every Wayland clipboard change to X11, so this is
            // reliable there; KWin only mirrors while an X11 window is focused.
            ensure_xwayland_auth();
            match x11::start_watch(tx.clone(), config) {
                Ok(()) => {
                    log::info!("Using XWayland clipboard monitoring");
                    return Ok(());
                }
                Err(e) => log::warn!("XWayland clipboard monitoring unavailable: {e}"),
            }

            // Last resort: polling — misses copies made less than 5s apart
            log::warn!("No event-driven clipboard backend available, using polling fallback");
            fallback::start_watch(tx, config)
        }
        Backend::X11 => x11::start_watch(tx, config),
    }
}

/// Make sure we can authenticate to Xwayland. When klipd runs as a systemd
/// user service, `DISPLAY`/`XAUTHORITY` may be missing from its environment;
/// without them the XFixes backend fails and we'd drop to lossy polling.
/// Compositors keep the Xwayland auth file in `$XDG_RUNTIME_DIR`
/// (Mutter: `.mutter-Xwaylandauth.*`, KWin: `xauth_*`), so look for it there.
fn ensure_xwayland_auth() {
    if std::env::var_os("DISPLAY").is_none() {
        std::env::set_var("DISPLAY", ":0");
        log::info!("DISPLAY not set, assuming :0 for XWayland");
    }
    if std::env::var_os("XAUTHORITY").is_some() {
        return;
    }
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let newest = std::fs::read_dir(runtime_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".mutter-Xwaylandauth.") || name.starts_with("xauth_")
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    if let Some(entry) = newest {
        log::info!("XAUTHORITY not set, using {:?}", entry.path());
        std::env::set_var("XAUTHORITY", entry.path());
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

/// Image types read via `wl-paste`, in priority order.
const WL_PASTE_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp"];

/// Read the clipboard via `wl-paste`: text if any text type is offered,
/// otherwise an image when `max_image_bytes` is `Some` (capture enabled).
/// Uses a timeout so a stuck source client can't hang the watcher.
pub fn read_clipboard_wl_paste(max_image_bytes: Option<usize>) -> Option<Clip> {
    let types = get_clipboard_types()?;
    let has_text = types.lines().any(|l| {
        l.starts_with("text/") || l == "UTF8_STRING" || l == "STRING" || l == "TEXT"
    });
    if has_text {
        let bytes = read_wl_paste_timeout(&["--no-newline"])
            .or_else(|| read_wl_paste_timeout(&[]))?;
        let content = String::from_utf8_lossy(&bytes).into_owned();
        return (!content.trim().is_empty()).then_some(Clip::Text(content));
    }
    let max = max_image_bytes?;
    let mime = WL_PASTE_IMAGE_MIMES.iter().find(|m| types.lines().any(|l| l == **m))?;
    let data = read_wl_paste_timeout(&["--type", mime])?;
    (!data.is_empty() && data.len() <= max)
        .then(|| Clip::Image { mime: mime.to_string(), data })
}

/// Run wl-paste with a 2-second timeout. Returns None if it times out or
/// fails. Content is returned exactly as copied (no trimming).
fn read_wl_paste_timeout(args: &[&str]) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut child = std::process::Command::new("wl-paste")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // Drain stdout on a separate thread: a clip larger than the pipe buffer
    // (64 KB) would otherwise block wl-paste forever and hit the timeout.
    let mut stdout = child.stdout.take()?;
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = done_tx.send(stdout.read_to_end(&mut buf).map(|_| buf));
    });

    let bytes = match done_rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(bytes)) => bytes,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    if !child.wait().ok()?.success() {
        return None;
    }
    Some(bytes)
}

/// Something a watcher backend captured from the clipboard.
#[derive(Clone, PartialEq)]
pub enum Clip {
    Text(String),
    Image { mime: String, data: Vec<u8> },
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