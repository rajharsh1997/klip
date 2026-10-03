use klip_common::Config;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

/// Fallback polling watcher — used when event-driven methods aren't available.
/// Polls every 5 seconds via `wl-paste` (text, or images when enabled).
pub fn start_watch(tx: Sender<super::Clip>, config: &Config) -> Result<(), anyhow::Error> {
    let max_image_bytes = config.capture_images.then(|| config.max_image_bytes());
    thread::spawn(move || {
        log::info!("Polling clipboard watcher started (every 5s)");
        let mut last_content: Option<super::Clip> = None;

        loop {
            // Reads text if offered, else an image; None for empty/unsupported
            match super::read_clipboard_wl_paste(max_image_bytes) {
                Some(clip) if Some(&clip) != last_content.as_ref() => {
                    last_content = Some(clip.clone());
                    let _ = tx.send(clip);
                }
                Some(_) => {}
                None => last_content = None,
            }

            thread::sleep(Duration::from_millis(5000));
        }
    });

    Ok(())
}
