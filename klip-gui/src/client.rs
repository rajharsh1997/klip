use klip_common::{DaemonRequest, DaemonResponse, ClipEntry};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Connect to the daemon's Unix socket and send a request, returning the response.
pub fn send_request(request: &DaemonRequest, socket_path: &PathBuf) -> Result<DaemonResponse, String> {
    let stream = UnixStream::connect(socket_path).map_err(|e| format!("Cannot connect to klip daemon: {}", e))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    let json = serde_json::to_string(request).map_err(|e| e.to_string())?;
    writer.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    writer.write_all(b"\n").map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;

    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(&line.trim()).map_err(|e| e.to_string())
}

/// Fetch all entries from the daemon.
pub fn list_entries(query: Option<&str>, socket_path: &PathBuf) -> Result<Vec<ClipEntry>, String> {
    match send_request(&DaemonRequest::List { query: query.map(String::from) }, socket_path)? {
        DaemonResponse::Entries(entries) => Ok(entries),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}

/// Toggle pin status of an entry.
pub fn toggle_pin(id: i64, socket_path: &PathBuf) -> Result<(), String> {
    match send_request(&DaemonRequest::TogglePin { id }, socket_path)? {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}

/// Delete an entry.
pub fn delete_entry(id: i64, socket_path: &PathBuf) -> Result<(), String> {
    match send_request(&DaemonRequest::Delete { id }, socket_path)? {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}

/// Copy an entry back to clipboard.
pub fn copy_entry(id: i64, socket_path: &PathBuf) -> Result<(), String> {
    match send_request(&DaemonRequest::Copy { id }, socket_path)? {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}

/// Clear unpinned history.
pub fn clear_history(socket_path: &PathBuf) -> Result<(), String> {
    match send_request(&DaemonRequest::ClearHistory, socket_path)? {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}
/// Open an event subscription. Returns a reader yielding one JSON
/// `DaemonEvent` per line, plus a handle whose `shutdown` ends the stream.
pub fn subscribe(socket_path: &PathBuf) -> Result<(BufReader<UnixStream>, UnixStream), String> {
    let stream = UnixStream::connect(socket_path).map_err(|e| format!("Cannot connect to klip daemon: {}", e))?;
    let handle = stream.try_clone().map_err(|e| e.to_string())?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    let json = serde_json::to_string(&DaemonRequest::Subscribe).map_err(|e| e.to_string())?;
    writer.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    writer.write_all(b"\n").map_err(|e| e.to_string())?;

    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    match serde_json::from_str(line.trim()).map_err(|e| e.to_string())? {
        DaemonResponse::Ok => Ok((reader, handle)),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}

/// Pause or resume clipboard capture.
pub fn set_paused(paused: bool, socket_path: &PathBuf) -> Result<(), String> {
    match send_request(&DaemonRequest::SetPaused { paused }, socket_path)? {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}

/// Entry count and whether capture is paused.
pub fn status(socket_path: &PathBuf) -> Result<(usize, bool), String> {
    match send_request(&DaemonRequest::Status, socket_path)? {
        DaemonResponse::Status { count, paused } => Ok((count, paused)),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}

/// Total number of stored entries.
pub fn count(socket_path: &PathBuf) -> Result<usize, String> {
    match send_request(&DaemonRequest::Count, socket_path)? {
        DaemonResponse::Count(n) => Ok(n),
        DaemonResponse::Error(e) => Err(e),
        _ => Err("Unexpected response".into()),
    }
}
