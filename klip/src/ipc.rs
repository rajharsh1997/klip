use crate::storage::Storage;
use anyhow::Result;
use klip_common::{DaemonEvent, DaemonRequest, DaemonResponse};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Path for the Unix domain socket.
pub fn socket_path(data_dir: &PathBuf) -> PathBuf {
    data_dir.join("klip.sock")
}

/// Fan-out of [`DaemonEvent`]s to every client that sent `Subscribe`.
#[derive(Default)]
pub struct Events {
    subscribers: Mutex<Vec<UnixStream>>,
}

impl Events {
    pub fn emit(&self, event: &DaemonEvent) {
        let Ok(mut line) = serde_json::to_string(event) else {
            return;
        };
        line.push('\n');
        // A write error means the client went away (or stalled past the write
        // timeout) — drop it.
        self.subscribers
            .lock()
            .unwrap()
            .retain_mut(|s| s.write_all(line.as_bytes()).is_ok());
    }

    fn add(&self, stream: UnixStream) -> Result<()> {
        // Never let a stuck subscriber block the daemon
        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
        self.subscribers.lock().unwrap().push(stream);
        Ok(())
    }
}

/// Run the IPC server, accepting connections from the GUI.
/// Uses synchronous I/O in threads.
pub fn run_ipc(
    listener: UnixListener,
    storage: Arc<Storage>,
    events: Arc<Events>,
    paused: Arc<AtomicBool>,
) -> Result<()> {
    loop {
        match listener.accept() {
            Ok((conn, _addr)) => {
                let storage = storage.clone();
                let events = events.clone();
                let paused = paused.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(conn, storage, events, paused) {
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
    events: Arc<Events>,
    paused: Arc<AtomicBool>,
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

        let (response, subscribe) = match serde_json::from_str::<DaemonRequest>(trimmed) {
            Ok(DaemonRequest::Subscribe) => (DaemonResponse::Ok, true),
            Ok(request) => (process_request(request, &storage, &events, &paused), false),
            Err(e) => {
                log::warn!("Failed to parse IPC request: {} (raw: {:?})", e, trimmed);
                (DaemonResponse::Error(format!("Parse error: {}", e)), false)
            }
        };
        let json = serde_json::to_string(&response)?;
        writer.write_all(json.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;

        if subscribe {
            // From now on this connection only receives events
            events.add(writer)?;
            return Ok(());
        }
    }
    Ok(())
}

fn process_request(
    request: DaemonRequest,
    storage: &Storage,
    events: &Events,
    paused: &AtomicBool,
) -> DaemonResponse {
    let result = match request {
        DaemonRequest::List { query } => {
            return match storage.list(query.as_deref()) {
                Ok(entries) => DaemonResponse::Entries(entries),
                Err(e) => DaemonResponse::Error(e.to_string()),
            }
        }
        DaemonRequest::Count => {
            return match storage.count() {
                Ok(c) => DaemonResponse::Count(c),
                Err(e) => DaemonResponse::Error(e.to_string()),
            }
        }
        DaemonRequest::Status => {
            return match storage.count() {
                Ok(count) => DaemonResponse::Status { count, paused: paused.load(Ordering::Relaxed) },
                Err(e) => DaemonResponse::Error(e.to_string()),
            }
        }
        DaemonRequest::SetPaused { paused: p } => {
            if paused.swap(p, Ordering::Relaxed) != p {
                log::info!("Capture {}", if p { "paused" } else { "resumed" });
                events.emit(&DaemonEvent::PausedChanged(p));
            }
            Ok(())
        }
        DaemonRequest::TogglePin { id } => storage
            .toggle_pin(id)
            .map(|entry| events.emit(&DaemonEvent::EntryUpdated(entry))),
        DaemonRequest::Delete { id } => storage
            .delete(id)
            .map(|()| events.emit(&DaemonEvent::EntryRemoved(id))),
        DaemonRequest::ClearHistory => storage
            .clear_history()
            .map(|_| events.emit(&DaemonEvent::HistoryCleared)),
        DaemonRequest::Copy { id } => storage.get_by_id(id).and_then(|entry| {
            let (data, mime) = if entry.is_image() {
                (storage.image_data(&entry)?, entry.mime_type.as_str())
            } else {
                (entry.content.into_bytes(), "text/plain")
            };
            copy_to_clipboard(&data, mime).map_err(|e| anyhow::anyhow!("Failed to copy: {e}"))
        }),
        DaemonRequest::Subscribe => unreachable!("handled in handle_client"),
    };
    match result {
        Ok(()) => DaemonResponse::Ok,
        Err(e) => DaemonResponse::Error(e.to_string()),
    }
}

/// Put `data` on the system clipboard. `mime` is `text/plain` or an image type.
fn copy_to_clipboard(data: &[u8], mime: &str) -> Result<()> {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        // Try wl-clipboard-rs (data-control protocol) first
        match copy_via_data_control(data, mime) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::debug!("data-control copy failed, trying wl-copy: {}", e);
            }
        }
        // Fallback: wl-copy (standard Wayland protocol, works on all compositors)
        let mut cmd = std::process::Command::new("wl-copy");
        if mime != "text/plain" {
            cmd.args(["--type", mime]);
        }
        pipe_to(cmd, data)
    } else {
        // X11 via xclip, which forks a background process to own the selection
        let mut cmd = std::process::Command::new("xclip");
        cmd.args(["-selection", "clipboard"]);
        if mime != "text/plain" {
            cmd.args(["-t", mime]);
        }
        pipe_to(cmd, data)
    }
}

/// Run `cmd` with `data` on stdin and wait for it to exit.
fn pipe_to(mut cmd: std::process::Command, data: &[u8]) -> Result<()> {
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(data)?;
    }
    child.wait()?;
    Ok(())
}

fn copy_via_data_control(data: &[u8], mime: &str) -> Result<()> {
    use wl_clipboard_rs::copy::{copy, MimeType, Options, Source};
    let source = Source::Bytes(data.to_vec().into_boxed_slice());
    let mime_type = if mime == "text/plain" {
        MimeType::Text
    } else {
        MimeType::Specific(mime.to_string())
    };
    copy(Options::new(), source, mime_type)?;
    Ok(())
}
