use klip_common::ClipEntry;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

/// Fallback polling watcher — used when event-driven methods aren't available.
/// Polls every 5 seconds using `wl-paste --list-types` (instant, no data transfer)
/// and only reads full content when the available types change.
pub fn start_watch(tx: Sender<ClipEntry>) -> Result<(), anyhow::Error> {
    thread::spawn(move || {
        log::info!("Polling clipboard watcher started (every 5s)");
        let mut last_types: Option<String> = None;
        let mut last_content: Option<String> = None;

        loop {
            // Step 1: cheap check — list available MIME types (instant, no data)
            let current_types = super::get_clipboard_types();
            if current_types == last_types {
                thread::sleep(Duration::from_millis(5000));
                continue;
            }

            // Types changed — clipboard may have new content. Check if text is available.
            let has_text = current_types.as_deref().map_or(false, |t| {
                t.lines().any(|l| {
                    l.starts_with("text/")
                        || l == "UTF8_STRING"
                        || l == "STRING"
                        || l == "TEXT"
                })
            });
            last_types = current_types;

            if !has_text {
                last_content = None;
                thread::sleep(Duration::from_millis(5000));
                continue;
            }

            // Step 2: read the actual text content
            if let Some(content) = super::read_clipboard_wl_paste() {
                if Some(&content) != last_content.as_ref() {
                    last_content = Some(content.clone());
                    let _ = tx.send(super::make_entry(content));
                }
            } else {
                last_content = None;
            }

            thread::sleep(Duration::from_millis(5000));
        }
    });

    Ok(())
}