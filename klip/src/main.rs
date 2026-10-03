mod ipc;
mod storage;
mod watcher;

use anyhow::Result;
use klip_common::{Config, DaemonEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use watcher::Clip;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let data_dir = klip_common::data_dir();
    std::fs::create_dir_all(&data_dir)?;

    // Single instance: if another daemon is already serving the socket, don't
    // steal it (two daemons would both record every clip).
    let socket_path = ipc::socket_path(&data_dir);
    if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
        log::info!("klipd is already running ({:?}), exiting", socket_path);
        return Ok(());
    }

    log::info!("Klip daemon starting...");
    log::info!("Data directory: {:?}", data_dir);

    if let Err(e) = Config::write_default_if_missing() {
        log::warn!("Could not write default config: {e}");
    }
    let (config, warning) = Config::load();
    if let Some(w) = warning {
        log::warn!("{w} — using defaults");
    }
    log::info!("Config: {:?}", config);

    // Initialize storage
    let storage = Arc::new(storage::Storage::new(data_dir.clone(), klip_common::images_dir())?);
    match storage.prune(config.max_history) {
        Ok(removed) if !removed.is_empty() => {
            log::info!("Pruned {} entries beyond max_history={}", removed.len(), config.max_history)
        }
        Ok(_) => {}
        Err(e) => log::error!("Failed to prune history: {e}"),
    }
    let events = Arc::new(ipc::Events::default());
    let paused = Arc::new(AtomicBool::new(false));

    // Channel: watcher -> storage processor
    let (clip_tx, clip_rx) = mpsc::channel::<Clip>();

    // Spawn clipboard watcher in a background thread
    let watcher_config = config.clone();
    std::thread::spawn(move || {
        if let Err(e) = watcher::start_watcher(clip_tx, &watcher_config) {
            log::error!("Clipboard watcher failed: {}", e);
        }
    });

    // Spawn storage processor: persists clips, enforces max_history, and
    // pushes events to subscribed clients
    let storage_for_processor = storage.clone();
    let events_for_processor = events.clone();
    let paused_for_processor = paused.clone();
    std::thread::spawn(move || {
        while let Ok(clip) = clip_rx.recv() {
            if paused_for_processor.load(Ordering::Relaxed) {
                log::debug!("Capture paused, dropping clip");
                continue;
            }
            let result = match clip {
                Clip::Text(content) => {
                    let mime = watcher::detect_content_type(&content);
                    storage_for_processor.insert(&content, &mime)
                }
                Clip::Image { .. } if !config.capture_images => continue,
                Clip::Image { data, .. } if data.len() > config.max_image_bytes() => {
                    log::info!("Skipping {} byte image (max_image_mb={})", data.len(), config.max_image_mb);
                    continue;
                }
                Clip::Image { mime, data } => storage_for_processor.insert_image(&mime, &data),
            };
            let inserted = match result {
                Ok(inserted) => inserted,
                Err(e) => {
                    log::error!("Failed to save clip: {}", e);
                    continue;
                }
            };
            let saved = inserted.entry;
            log::info!("Clip saved: id={}, type={}, new={}", saved.id, saved.mime_type, inserted.is_new);
            events_for_processor.emit(&if inserted.is_new {
                DaemonEvent::EntryAdded(saved)
            } else {
                DaemonEvent::EntryUpdated(saved)
            });

            match storage_for_processor.prune(config.max_history) {
                Ok(removed) => {
                    for id in removed {
                        events_for_processor.emit(&DaemonEvent::EntryRemoved(id));
                    }
                }
                Err(e) => log::error!("Failed to prune history: {e}"),
            }
        }
    });

    // Start IPC server — any socket file left at this point is stale
    let _ = std::fs::remove_file(&socket_path);

    let listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
    log::info!("IPC socket at {:?}", socket_path);

    ipc::run_ipc(listener, storage, events, paused)?;

    Ok(())
}
