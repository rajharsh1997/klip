mod ipc;
mod storage;
mod watcher;

use anyhow::Result;
use klip_common::{ClipEntry, DaemonEvent};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

/// Default data directory: ~/.local/share/klip
fn default_data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("klip")
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let data_dir = default_data_dir();
    std::fs::create_dir_all(&data_dir)?;

    log::info!("Klip daemon starting...");
    log::info!("Data directory: {:?}", data_dir);

    // Initialize storage
    let storage = Arc::new(storage::Storage::new(data_dir.clone())?);
    let storage_clone = storage.clone();

    // Channel: watcher -> daemon (ClipEntry)
    let (clip_tx, clip_rx) = mpsc::channel::<ClipEntry>();
    // Channel: daemon -> IPC clients (DaemonEvent)
    let (event_tx, event_rx) = mpsc::channel::<DaemonEvent>();

    // Spawn clipboard watcher in a background thread
    std::thread::spawn(move || {
        if let Err(e) = watcher::start_watcher(clip_tx) {
            log::error!("Clipboard watcher failed: {}", e);
        }
    });

    // Spawn storage processor: reads from clip_rx, persists, and forwards events
    let storage_for_processor = storage_clone.clone();
    let event_tx_for_processor = event_tx.clone();
    std::thread::spawn(move || {
        while let Ok(entry) = clip_rx.recv() {
            match storage_for_processor.insert(&entry.content, &entry.mime_type) {
                Ok(saved) => {
                    log::info!("New clip saved: id={}, len={}", saved.id, saved.content.len());
                    let _ = event_tx_for_processor.send(DaemonEvent::EntryAdded(saved));
                }
                Err(e) => log::error!("Failed to save clip: {}", e),
            }
        }
    });

    // Start IPC server
    let socket_path = ipc::socket_path(&data_dir);
    // Remove stale socket if present
    let _ = std::fs::remove_file(&socket_path);

    let listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
    log::info!("IPC socket at {:?}", socket_path);

    let event_rx = Arc::new(std::sync::Mutex::new(event_rx));
    ipc::run_ipc(listener, storage, event_rx)?;

    Ok(())
}