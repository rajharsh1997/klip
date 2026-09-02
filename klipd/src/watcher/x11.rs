use klip_common::ClipEntry;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::rust_connection::RustConnection;

/// Start X11 clipboard monitoring via selection owner tracking.
/// Only fires when the clipboard owner actually changes (event-driven).
pub fn start_watch(tx: Sender<ClipEntry>) -> Result<(), anyhow::Error> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen_root = conn.setup().roots[screen_num].root;

    let clipboard_atom = conn
        .intern_atom(false, b"CLIPBOARD")?
        .reply()?
        .atom;
    let utf8_string_atom = conn
        .intern_atom(false, b"UTF8_STRING")?
        .reply()?
        .atom;

    // Move conn into the thread
    thread::spawn(move || {
        log::info!("X11 clipboard watcher started");
        let mut last_owner = 0;

        loop {
            let owner = match conn.get_selection_owner(clipboard_atom) {
                Ok(cookie) => match cookie.reply() {
                    Ok(r) => r.owner,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(1000));
                        continue;
                    }
                },
                Err(_) => {
                    thread::sleep(Duration::from_millis(1000));
                    continue;
                }
            };

            if owner != last_owner && owner != 0 {
                last_owner = owner;

                if let Err(e) = conn.convert_selection(
                    screen_root,
                    clipboard_atom,
                    utf8_string_atom,
                    clipboard_atom,
                    0u32,
                ) {
                    log::debug!("convert_selection failed: {}", e);
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }

                let _ = conn.flush();
                thread::sleep(Duration::from_millis(200));

                if let Ok(cookie) = conn.get_property(false, screen_root, clipboard_atom, 0u32, 0u32, 1_000_000) {
                    if let Ok(reply) = cookie.reply() {
                        if !reply.value.is_empty() {
                            let content = String::from_utf8_lossy(&reply.value).to_string();
                            if !content.is_empty() {
                                let _ = tx.send(ClipEntry {
                                    id: 0,
                                    content,
                                    mime_type: "text/plain".into(),
                                    pinned: false,
                                    created_at: String::new(),
                                    updated_at: String::new(),
                                });
                            }
                        }
                    }
                }
            }

            thread::sleep(Duration::from_millis(500));
        }
    });

    Ok(())
}