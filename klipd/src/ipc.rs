use crate::storage::Storage;
use anyhow::Result;
use klip_common::{DaemonEvent, DaemonRequest, DaemonResponse};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;

/// Path for the Unix domain socket.
pub fn socket_path(data_dir: &PathBuf) -> PathBuf {
    data_dir.join("klip.sock")
}

/// Run the IPC server, accepting connections from the GUI.
/// Uses synchronous I/O in threads.
pub fn run_ipc(
    listener: UnixListener,
    storage: Arc<Storage>,
    event_rx: Arc<std::sync::Mutex<Receiver<DaemonEvent>>>,
) -> Result<()> {
    loop {
        match listener.accept() {
            Ok((conn, _addr)) => {
                let storage = storage.clone();
                let event_rx = event_rx.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(conn, storage, event_rx) {
                        log::error!("Client handler error: {}", e);
                    }
                });
            }
            Err(e) => {
                log::error!("IPC accept error: {}", e);
                thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}

fn handle_client(
    conn: UnixStream,
    storage: Arc<Storage>,
    event_rx: Arc<std::sync::Mutex<Receiver<DaemonEvent>>>,
) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut writer = conn;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<DaemonRequest>(trimmed) {
            Ok(request) => process_request(request, &storage, &event_rx),
            Err(e) => {
                log::warn!("Failed to parse IPC request: {} (raw: {:?})", e, trimmed);
                DaemonResponse::Error(format!("Parse error: {}", e))
            }
        };
        let json = serde_json::to_string(&response)?;
        writer.write_all(json.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

fn process_request(
    request: DaemonRequest,
    storage: &Arc<Storage>,
    event_rx: &Arc<std::sync::Mutex<Receiver<DaemonEvent>>>,
) -> DaemonResponse {
    // Drain any pending events (non-blocking)
    if let Ok(rx) = event_rx.try_lock() {
        while let Ok(_evt) = rx.try_recv() {
            // Events are consumed here; could broadcast to clients in future
        }
    }

    match request {
        DaemonRequest::List { query } => match storage.list(query.as_deref()) {
            Ok(entries) => DaemonResponse::Entries(entries),
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::TogglePin { id } => match storage.toggle_pin(id) {
            Ok(_) => DaemonResponse::Ok,
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::Delete { id } => match storage.delete(id) {
            Ok(_) => DaemonResponse::Ok,
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::ClearHistory => match storage.clear_history() {
            Ok(_) => DaemonResponse::Ok,
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::Copy { id } => match storage.get_by_id(id) {
            Ok(entry) => {
                // Copy content to system clipboard
                if let Err(e) = copy_to_clipboard(&entry.content) {
                    return DaemonResponse::Error(format!("Failed to copy: {}", e));
                }
                DaemonResponse::Ok
            }
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::Count => match storage.count() {
            Ok(c) => DaemonResponse::Count(c),
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
    }
}

fn copy_to_clipboard(content: &str) -> Result<()> {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        // Try wl-clipboard-rs (wlr-data-control protocol) first
        match copy_via_wlr(content) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::debug!("wlr-data-control copy failed, trying wl-copy: {}", e);
            }
        }
        // Fallback: wl-copy (standard Wayland protocol, works on all compositors)
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
    } else {
        // X11 via xclip
        let output = std::process::Command::new("xclip")
            .arg("-selection")
            .arg("clipboard")
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = output.stdin {
            use std::io::Write;
            stdin.write_all(content.as_bytes())?;
        }
        Ok(())
    }
}

fn copy_via_wlr(content: &str) -> Result<()> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source, copy};
    let source = Source::Bytes(content.as_bytes().to_vec().into_boxed_slice());
    let options = Options::new();
    copy(options, source, MimeType::Text)?;
    Ok(())
}