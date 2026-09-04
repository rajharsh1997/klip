//! Event-driven X11 clipboard monitoring via the XFixes extension.
//!
//! Instead of polling `GetSelectionOwner` every 500ms, we register with the
//! X server using `XFixesSelectSelectionInput`. The server then pushes an
//! `XFixesSelectionNotifyEvent` whenever the CLIPBOARD selection owner changes.
//! Zero CPU when idle — the thread parks in `wait_for_event()`.
//!
//! Event flow:
//!   1. Create an InputOnly dummy window (to receive events)
//!   2. `xfixes_select_selection_input(window, CLIPBOARD, SET_SELECTION_OWNER)`
//!   3. `wait_for_event()` — blocks in kernel until X server sends something
//!   4. On `XfixesSelectionNotify { subtype: SET_SELECTION_OWNER }`:
//!        → `convert_selection(window, CLIPBOARD, UTF8_STRING, prop, timestamp)`
//!   5. On `SelectionNotify`:
//!        → `get_property(window, prop)` → read text → send to channel
//!
//! Falls back to the original polling approach if XFixes is not available
//! (extremely old X servers only).

use anyhow::Result;
use klip_common::ClipEntry;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as XFixesExt};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, WindowClass};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::NONE;

pub fn start_watch(tx: Sender<ClipEntry>) -> Result<()> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let screen_root = screen.root;

    // ── Check XFixes availability ─────────────────────────────────────────────
    let ext_info = conn.query_extension(b"XFIXES")?.reply()?;
    if !ext_info.present {
        log::warn!("XFixes extension not available — falling back to polling");
        return start_watch_polling(conn, screen_root, tx);
    }
    // Initialize the extension (required before using any XFixes request)
    conn.xfixes_query_version(5, 0)?.reply()?;
    log::info!("X11 XFixes extension available — using event-driven clipboard monitoring");

    // ── Intern atoms ──────────────────────────────────────────────────────────
    let clipboard_atom = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    let utf8_atom = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
    // We store converted selection data in this custom property on our window
    let prop_atom = conn.intern_atom(false, b"_KLIP_CLIPBOARD")?.reply()?.atom;

    // ── Create a tiny InputOnly window to own our requests ────────────────────
    let win = conn.generate_id()?;
    conn.create_window(
        0,          // depth: CopyFromParent
        win,
        screen_root,
        -10, -10, 1, 1, // off-screen, 1×1
        0,          // border_width
        WindowClass::INPUT_ONLY,
        0,          // visual: CopyFromParent
        &CreateWindowAux::new(),
    )?.check()?;

    // ── Subscribe to CLIPBOARD owner-change events ────────────────────────────
    conn.xfixes_select_selection_input(
        win,
        clipboard_atom,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER,
    )?.check()?;

    conn.flush()?;

    // ── Background thread — blocks until X server pushes events ──────────────
    thread::spawn(move || {
        log::info!("X11 XFixes clipboard watcher active");
        let mut last_content: Option<String> = None;

        loop {
            let event = match conn.wait_for_event() {
                Ok(e) => e,
                Err(e) => {
                    log::error!("[x11] wait_for_event error: {e}");
                    break;
                }
            };

            match event {
                // ── CLIPBOARD owner changed ───────────────────────────────────
                Event::XfixesSelectionNotify(e) => {
                    // Only care about CLIPBOARD (not PRIMARY or other selections)
                    if e.selection != clipboard_atom {
                        continue;
                    }
                    // Only SET_SELECTION_OWNER events (not destroy/client-close)
                    if e.subtype != xfixes::SelectionEvent::SET_SELECTION_OWNER {
                        continue;
                    }
                    // If clipboard was cleared (no owner), ignore
                    if e.owner == NONE {
                        continue;
                    }

                    log::debug!("[x11] CLIPBOARD owner changed (owner={:#x})", e.owner);

                    // Ask the new owner to convert selection to UTF8_STRING.
                    // Use the event's timestamp so the owner knows this is a
                    // valid request and not a replay.
                    if let Err(err) = conn.convert_selection(
                        win,
                        clipboard_atom,
                        utf8_atom,
                        prop_atom,
                        e.timestamp,
                    ) {
                        log::debug!("[x11] convert_selection failed: {err}");
                        continue;
                    }
                    let _ = conn.flush();
                }

                // ── Selection owner responded with data ───────────────────────
                Event::SelectionNotify(e) => {
                    // `property == NONE` means the owner refused / couldn't convert
                    if e.property == NONE {
                        log::debug!("[x11] SelectionNotify: owner refused conversion");
                        continue;
                    }

                    // Read the data the owner wrote to our window's property
                    match conn.get_property(
                        true,       // delete: clean up the property after reading
                        win,
                        prop_atom,
                        AtomEnum::ANY,
                        0,
                        u32::MAX / 4,
                    ) {
                        Ok(cookie) => match cookie.reply() {
                            Ok(reply) if !reply.value.is_empty() => {
                                let content =
                                    String::from_utf8_lossy(&reply.value).into_owned();
                                if !content.is_empty()
                                    && Some(&content) != last_content.as_ref()
                                {
                                    log::debug!(
                                        "[x11] new clip ({} bytes)",
                                        content.len()
                                    );
                                    last_content = Some(content.clone());
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
                            Ok(_) => {}  // empty property — binary/non-text clip
                            Err(e) => log::debug!("[x11] get_property reply error: {e}"),
                        },
                        Err(e) => log::debug!("[x11] get_property request error: {e}"),
                    }
                }

                _ => {} // ignore all other events
            }
        }

        log::warn!("[x11] XFixes watcher thread exited");
    });

    Ok(())
}

// ── Polling fallback (XFixes not available) ───────────────────────────────────
//
// Kept for completeness on very old X servers. Polls GetSelectionOwner
// every 500ms, same as the original implementation.

fn start_watch_polling(
    conn: RustConnection,
    screen_root: u32,
    tx: Sender<ClipEntry>,
) -> Result<()> {
    let clipboard_atom = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    let utf8_atom = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;

    thread::spawn(move || {
        log::info!("[x11] Polling clipboard watcher started (500ms, XFixes unavailable)");
        let mut last_owner = 0u32;

        loop {
            let owner = match conn.get_selection_owner(clipboard_atom) {
                Ok(cookie) => match cookie.reply() {
                    Ok(r) => r.owner,
                    Err(_) => { thread::sleep(Duration::from_millis(1000)); continue; }
                },
                Err(_) => { thread::sleep(Duration::from_millis(1000)); continue; }
            };

            if owner != last_owner && owner != 0 {
                last_owner = owner;

                if let Err(e) = conn.convert_selection(
                    screen_root,
                    clipboard_atom,
                    utf8_atom,
                    clipboard_atom,
                    0u32,
                ) {
                    log::debug!("[x11] convert_selection failed: {e}");
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
                let _ = conn.flush();
                thread::sleep(Duration::from_millis(200));

                if let Ok(cookie) = conn.get_property(
                    false,
                    screen_root,
                    clipboard_atom,
                    0u32,
                    0u32,
                    1_000_000,
                ) {
                    if let Ok(reply) = cookie.reply() {
                        if !reply.value.is_empty() {
                            let content =
                                String::from_utf8_lossy(&reply.value).to_string();
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