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
//!        → `convert_selection(window, CLIPBOARD, TARGETS, prop, timestamp)`
//!   5. On `SelectionNotify` for TARGETS: pick text if offered, else an image
//!        type → `convert_selection` to it
//!   6. On `SelectionNotify` for the data: `get_property(window, prop)`. If its
//!        type is INCR (large data), collect chunks on `PropertyNotify` until an
//!        empty one arrives. Send the result to the channel.
//!
//! Falls back to the original polling approach if XFixes is not available
//! (extremely old X servers only).

use anyhow::Result;
use klip_common::Config;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as XFixesExt};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, GetPropertyReply, Property,
    Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::NONE;

/// Text targets in priority order. `UTF8_STRING` is what nearly every owner
/// offers; `STRING`/`TEXT` are legacy (Latin-1, decoded lossily).
const TEXT_TARGETS: &[&str] = &["UTF8_STRING", "text/plain;charset=utf-8", "STRING", "TEXT"];
/// Image targets in priority order, used only when no text target is offered.
const IMAGE_TARGETS: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp"];
/// Ignore text clips larger than this.
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// Where the transfer of the current selection stands.
enum Transfer {
    Idle,
    /// Asked the owner which targets (formats) it offers.
    Targets { timestamp: u32 },
    /// Asked for the data in `target`. `mime` is `None` for text.
    Data { target: Atom, mime: Option<&'static str> },
    /// Receiving data in chunks (INCR protocol, used for large selections).
    Incr { mime: Option<&'static str>, buf: Vec<u8> },
}

struct Watcher {
    conn: RustConnection,
    win: Window,
    clipboard: Atom,
    targets: Atom,
    incr: Atom,
    /// Property on our window that owners write converted data into.
    prop: Atom,
    text_atoms: Vec<Atom>,
    image_atoms: Vec<(Atom, &'static str)>,
    /// Image size limit in bytes; `None` when image capture is disabled.
    max_image_bytes: Option<usize>,
    transfer: Transfer,
    /// Raw bytes of the last recorded clip, to skip re-announced selections.
    last_content: Option<Vec<u8>>,
    tx: Sender<super::Clip>,
}

pub fn start_watch(tx: Sender<super::Clip>, config: &Config) -> Result<()> {
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
    let intern = |name: &str| -> Result<Atom> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    };
    let clipboard = intern("CLIPBOARD")?;
    let targets = intern("TARGETS")?;
    let incr = intern("INCR")?;
    // We store converted selection data in this custom property on our window
    let prop = intern("_KLIP_CLIPBOARD")?;
    let text_atoms = TEXT_TARGETS.iter().map(|t| intern(t)).collect::<Result<Vec<_>>>()?;
    let image_atoms = IMAGE_TARGETS
        .iter()
        .map(|&t| Ok((intern(t)?, t)))
        .collect::<Result<Vec<_>>>()?;

    // ── Create a tiny InputOnly window to own our requests ────────────────────
    // PropertyNotify events on it drive INCR (chunked) transfers.
    let win = conn.generate_id()?;
    conn.create_window(
        0,          // depth: CopyFromParent
        win,
        screen_root,
        -10, -10, 1, 1, // off-screen, 1×1
        0,          // border_width
        WindowClass::INPUT_ONLY,
        0,          // visual: CopyFromParent
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?.check()?;

    // ── Subscribe to CLIPBOARD owner-change events ────────────────────────────
    conn.xfixes_select_selection_input(
        win,
        clipboard,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER,
    )?.check()?;

    conn.flush()?;

    let mut watcher = Watcher {
        conn,
        win,
        clipboard,
        targets,
        incr,
        prop,
        text_atoms,
        image_atoms,
        max_image_bytes: config.capture_images.then(|| config.max_image_bytes()),
        transfer: Transfer::Idle,
        last_content: None,
        tx,
    };

    // ── Background thread — blocks until X server pushes events ──────────────
    thread::spawn(move || {
        log::info!("X11 XFixes clipboard watcher active");
        loop {
            match watcher.conn.wait_for_event() {
                Ok(event) => {
                    if let Err(e) = watcher.handle(event) {
                        log::debug!("[x11] {e}");
                        watcher.transfer = Transfer::Idle;
                    }
                }
                Err(e) => {
                    log::error!("[x11] wait_for_event error: {e}");
                    break;
                }
            }
        }
        log::warn!("[x11] XFixes watcher thread exited");
    });

    Ok(())
}

impl Watcher {
    /// Event flow: owner change → convert to TARGETS → pick a format →
    /// convert to it → read the property (directly, or chunk by chunk via INCR).
    fn handle(&mut self, event: Event) -> Result<()> {
        match event {
            // ── CLIPBOARD owner changed ───────────────────────────────────────
            Event::XfixesSelectionNotify(e) => {
                if e.selection != self.clipboard
                    || e.subtype != xfixes::SelectionEvent::SET_SELECTION_OWNER
                    || e.owner == NONE
                {
                    return Ok(());
                }
                log::debug!("[x11] CLIPBOARD owner changed (owner={:#x})", e.owner);
                // Use the event's timestamp so the owner knows this is a
                // valid request and not a replay. Abandons any transfer still
                // in progress for the previous owner.
                self.transfer = Transfer::Targets { timestamp: e.timestamp };
                self.convert(self.targets, e.timestamp)?;
            }

            // ── Owner answered a conversion request ───────────────────────────
            Event::SelectionNotify(e) => match self.transfer {
                Transfer::Targets { timestamp } if e.target == self.targets => {
                    let offered = if e.property == NONE {
                        Vec::new() // owner doesn't list targets; just try text
                    } else {
                        self.read_prop()?.value32().map(|v| v.collect()).unwrap_or_default()
                    };
                    let Some((target, mime)) = self.choose_target(&offered) else {
                        log::debug!("[x11] selection has no supported target");
                        self.transfer = Transfer::Idle;
                        return Ok(());
                    };
                    self.transfer = Transfer::Data { target, mime };
                    self.convert(target, timestamp)?;
                }
                Transfer::Data { target, mime } if e.target == target => {
                    self.transfer = Transfer::Idle;
                    if e.property == NONE {
                        log::debug!("[x11] owner refused conversion");
                        return Ok(());
                    }
                    // Reading with delete=true also tells an INCR owner to
                    // start sending chunks
                    let reply = self.read_prop()?;
                    if reply.type_ == self.incr {
                        log::debug!("[x11] INCR transfer started");
                        self.transfer = Transfer::Incr { mime, buf: Vec::new() };
                    } else {
                        self.finish(mime, reply.value);
                    }
                }
                _ => {}
            },

            // ── Next INCR chunk is ready ──────────────────────────────────────
            Event::PropertyNotify(e)
                if e.window == self.win
                    && e.atom == self.prop
                    && e.state == Property::NEW_VALUE
                    && matches!(self.transfer, Transfer::Incr { .. }) =>
            {
                let chunk = self.read_prop()?.value;
                let max_image_bytes = self.max_image_bytes;
                let Transfer::Incr { mime, buf } = &mut self.transfer else { unreachable!() };
                if chunk.is_empty() {
                    // Zero-length chunk marks the end
                    let (mime, data) = (*mime, std::mem::take(buf));
                    self.transfer = Transfer::Idle;
                    self.finish(mime, data);
                } else {
                    buf.extend_from_slice(&chunk);
                    if buf.len() > max_bytes(*mime, max_image_bytes) {
                        log::debug!("[x11] selection too large, abandoning");
                        self.transfer = Transfer::Idle;
                    }
                }
            }

            _ => {}
        }
        Ok(())
    }

    /// Ask the owner to write the selection in `target` format to our property.
    fn convert(&self, target: Atom, timestamp: u32) -> Result<()> {
        self.conn.convert_selection(self.win, self.clipboard, target, self.prop, timestamp)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Read and delete our property.
    fn read_prop(&self) -> Result<GetPropertyReply> {
        Ok(self
            .conn
            .get_property(true, self.win, self.prop, AtomEnum::ANY, 0, u32::MAX / 4)?
            .reply()?)
    }

    /// Prefer text; fall back to an image type. An empty `offered` list (owner
    /// doesn't support TARGETS) means "try UTF8_STRING".
    fn choose_target(&self, offered: &[Atom]) -> Option<(Atom, Option<&'static str>)> {
        if offered.is_empty() {
            return Some((self.text_atoms[0], None));
        }
        if let Some(&atom) = self.text_atoms.iter().find(|a| offered.contains(a)) {
            return Some((atom, None));
        }
        self.max_image_bytes?;
        self.image_atoms
            .iter()
            .find(|(a, _)| offered.contains(a))
            .map(|&(atom, mime)| (atom, Some(mime)))
    }

    fn finish(&mut self, mime: Option<&'static str>, data: Vec<u8>) {
        if data.is_empty()
            || data.len() > max_bytes(mime, self.max_image_bytes)
            || Some(&data) == self.last_content.as_ref()
        {
            return;
        }
        let clip = match mime {
            None => {
                let text = String::from_utf8_lossy(&data).into_owned();
                if text.trim().is_empty() {
                    return;
                }
                super::Clip::Text(text)
            }
            Some(mime) => super::Clip::Image { mime: mime.to_string(), data: data.clone() },
        };
        log::debug!("[x11] new {} clip ({} bytes)", mime.unwrap_or("text"), data.len());
        self.last_content = Some(data);
        let _ = self.tx.send(clip);
    }
}

fn max_bytes(mime: Option<&str>, max_image_bytes: Option<usize>) -> usize {
    match mime {
        None => MAX_TEXT_BYTES,
        Some(_) => max_image_bytes.unwrap_or(0),
    }
}

// ── Polling fallback (XFixes not available) ───────────────────────────────────
//
// Kept for completeness on very old X servers. Polls GetSelectionOwner
// every 500ms, same as the original implementation.

fn start_watch_polling(
    conn: RustConnection,
    screen_root: u32,
    tx: Sender<super::Clip>,
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
                                let _ = tx.send(super::Clip::Text(content));
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