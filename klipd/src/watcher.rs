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

/// Start polling the clipboard for changes.
/// Returns a channel sender so the daemon can push new entries to storage.
pub fn start_watcher(tx: Sender<ClipEntry>) -> Result<()> {
    let backend = detect_backend();
    log::info!("Starting clipboard watcher on {:?}", backend);

    match backend {
        Backend::Wayland => watch_wayland(tx),
        Backend::X11 => watch_x11(tx),
    }
}

/// Try to read clipboard via `wl-paste` command (standard Wayland protocol).
/// Works on all compositors including KDE Plasma.
fn read_clipboard_wl_paste() -> Option<String> {
    let output = std::process::Command::new("wl-paste")
        .arg("--no-newline")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        let content = String::from_utf8_lossy(&output.stdout).to_string();
        if !content.is_empty() {
            return Some(content);
        }
    }
    None
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

fn watch_wayland(tx: Sender<ClipEntry>) -> Result<()> {
    // Try to detect if wlr-data-control protocol is available by checking
    // if the compositor supports it via a quick probe
    let use_wlr_protocol = probe_wlr_data_control();

    if use_wlr_protocol {
        log::info!("Using wlr-data-control protocol for clipboard monitoring");
        watch_wayland_wlr(tx)
    } else {
        log::info!("wlr-data-control not available, falling back to wl-paste polling");
        watch_wayland_fallback(tx)
    }
}

/// Probe whether the compositor supports wlr-data-control protocol.
fn probe_wlr_data_control() -> bool {
    use wl_clipboard_rs::paste::{get_contents, ClipboardType, MimeType, Seat};
    match get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text) {
        Ok(_) => true,
        Err(e) => {
            log::debug!("wlr-data-control probe failed: {}", e);
            false
        }
    }
}

/// Watch clipboard using wlr-data-control protocol (fast, event-driven).
fn watch_wayland_wlr(tx: Sender<ClipEntry>) -> Result<()> {
    use std::io::Read;
    use wl_clipboard_rs::paste::{get_contents, ClipboardType, MimeType, Seat};

    let mut last_content: Option<String> = None;

    loop {
        match get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text) {
            Ok((mut pipe, _mime_type)) => {
                let mut buf = String::new();
                if pipe.read_to_string(&mut buf).is_ok() {
                    if Some(&buf) != last_content.as_ref() && !buf.is_empty() {
                        last_content = Some(buf.clone());
                        let entry = ClipEntry {
                            id: 0,
                            content: buf,
                            mime_type: "text/plain".into(),
                            pinned: false,
                            created_at: String::new(),
                            updated_at: String::new(),
                        };
                        let _ = tx.send(entry);
                    }
                }
            }
            Err(e) => log::warn!("Wayland clipboard read error: {}", e),
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Watch clipboard using `wl-paste` command fallback (works on all compositors).
fn watch_wayland_fallback(tx: Sender<ClipEntry>) -> Result<()> {
    let mut last_content: Option<String> = None;

    loop {
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
                let _ = tx.send(entry);
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn watch_x11(tx: Sender<ClipEntry>) -> Result<()> {
    use x11rb::connection::Connection;
    use x11rb::rust_connection::RustConnection;

    let (conn, _screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[_screen_num];

    // Track the clipboard atom
    let clipboard_atom = conn
        .intern_atom(false, b"CLIPBOARD")?
        .reply()?
        .atom;
    let utf8_string_atom = conn
        .intern_atom(false, b"UTF8_STRING")?
        .reply()?
        .atom;

    // We use the clipboard owner change as a trigger
    let mut last_owner = 0;

    loop {
        let owner = conn
            .get_selection_owner(clipboard_atom)?
            .reply()?
            .owner;

        if owner != last_owner && owner != 0 {
            last_owner = owner;

            // Request the clipboard content
            conn.convert_selection(
                screen.root,
                clipboard_atom,
                utf8_string_atom,
                clipboard_atom,
                0u32,
            )?;

            // Flush and wait a bit for the selection to arrive
            conn.flush()?;
            thread::sleep(Duration::from_millis(200));

            // Try to read the property
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