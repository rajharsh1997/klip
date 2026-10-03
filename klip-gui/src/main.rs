mod client;
mod row_menu;
mod tray;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use glib::translate::IntoGlib;
use klip_common::ClipEntry;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::rc::Rc;
fn daemon_alive(socket_path: &PathBuf) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

/// Poll for up to 2s until the daemon accepts connections.
fn wait_for_daemon(socket_path: &PathBuf) -> bool {
    for _ in 0..20 {
        if daemon_alive(socket_path) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Try to start the daemon if it's not already running.
fn ensure_daemon_running(socket_path: &PathBuf) {
    if daemon_alive(socket_path) {
        return;
    }
    eprintln!("[klip-gui] Daemon not running, starting it...");

    // Try systemd first (preferred — handles lifecycle, auto-restart, etc.)
    let systemd_ok = std::process::Command::new("systemctl")
        .args(["--user", "start", "klipd"])
        .status()
        .is_ok_and(|s| s.success());

    // `systemctl start` succeeds as soon as the process is forked, even if the
    // unit then fails (e.g. a wrong ExecStart path) — so verify the socket.
    if systemd_ok {
        if wait_for_daemon(socket_path) {
            eprintln!("[klip-gui] Daemon started via systemd");
            return;
        }
        eprintln!("[klip-gui] systemd unit started but daemon isn't responding, stopping it");
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", "klipd"])
            .status();
    }

    // Fallback: spawn daemon directly (non-systemd: static distros, containers, etc.)
    eprintln!("[klip-gui] Falling back to direct daemon spawn...");
    match std::process::Command::new("klipd")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) if wait_for_daemon(socket_path) => eprintln!("[klip-gui] Daemon started directly"),
        Ok(_) => eprintln!("[klip-gui] Daemon process spawned but not responding yet, proceeding..."),
        Err(e) => eprintln!("[klip-gui] Could not start daemon: {e}"),
    }
}

fn main() -> glib::ExitCode {
    // `--hidden` (used by the autostart entry) starts just the tray, without
    // opening the palette. Strip it so GTK doesn't reject the unknown option.
    let (hidden, args): (Vec<String>, Vec<String>) =
        std::env::args().partition(|a| a == "--hidden");
    let start_hidden = Cell::new(!hidden.is_empty());

    let app = gtk4::Application::new(
        Some("com.klip.clipboard-manager"),
        gio::ApplicationFlags::empty(),
    );

    // `startup` only fires in the primary instance. A second `klip` invocation
    // (e.g. from the global hotkey) just forwards `activate` over D-Bus and exits,
    // so creating the tray here avoids a temporary duplicate tray icon.
    app.connect_startup(|app| {
        std::mem::forget(app.hold()); // Keeps the GTK application alive in the background even when 0 windows exist
        load_css();
        tray::spawn_tray(app);
    });

    app.connect_activate(move |app| {
        let socket_path = klip_common::socket_path();
        ensure_daemon_running(&socket_path);
        if start_hidden.replace(false) {
            return;
        }
        if let Some(win) = app.active_window() {
            let ts = (glib::monotonic_time() / 1000) as u32;
            win.present_with_time(ts);
        } else {
            build_ui(app, socket_path);
        }
    });

    app.run_with_args(&args)
}

fn load_css() {
    let css = gtk4::CssProvider::new();
    css.load_from_data(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Maximum rows shown at once, to keep layout fast.
const MAX_RESULTS: usize = 50;
/// Bounding box for image thumbnails in the list.
const THUMB_W: i32 = 200;
const THUMB_H: i32 = 56;
/// Icon theme names tried in order (Adwaita, Breeze, generic).
const PIN_ICONS: &[&str] = &["view-pin-symbolic", "pin-symbolic", "starred-symbolic"];
const DELETE_ICONS: &[&str] = &["user-trash-symbolic", "edit-delete-symbolic"];

/// A thumbnail and the image's original width and height (`None` if unreadable).
type Thumb = Option<(gdk::Texture, i32, i32)>;

/// The open palette window and its state. Lives until the window is destroyed.
struct Palette {
    socket_path: PathBuf,
    window: gtk4::ApplicationWindow,
    search: gtk4::SearchEntry,
    scrolled: gtk4::ScrolledWindow,
    list_box: gtk4::ListBox,
    /// Everything fetched from the daemon (its 500 most recent entries).
    all_entries: RefCell<Vec<ClipEntry>>,
    /// The entries currently displayed, in order, with their rows.
    shown: RefCell<Vec<(ClipEntry, gtk4::ListBoxRow)>>,
    /// Decoded thumbnails, keyed by image file name.
    thumbs: RefCell<HashMap<String, Thumb>>,
    /// Whether the daemon pushes change events to us. If so, the list refreshes
    /// from those events rather than after each of our own actions.
    subscribed: Cell<bool>,
    /// Right-click menu (see `row_menu.rs`).
    row_menu: gtk4::Popover,
    /// While the menu is open the palette may look unfocused; don't dismiss it.
    menu_open: Rc<Cell<bool>>,
}

fn build_ui(app: &gtk4::Application, socket_path: PathBuf) {
    // ── Window ────────────────────────────────────────────────────────────────
    let window = gtk4::ApplicationWindow::new(app);
    window.set_title(Some("Klip"));
    window.set_default_size(320, 480);
    window.set_resizable(false);
    window.set_decorated(false);
    window.add_css_class("klip-popup");
    window.set_icon_name(Some("klip"));

    // Auto-dismiss once the compositor deactivates the window, with a small
    // debounce to ignore mapping glitches. This watches the toplevel's FOCUSED
    // state (xdg "activated"), not `is_active`: the latter also drops while
    // any popup holds the keyboard, e.g. a panel's tray context menu. Closing
    // then, while KWin still counts us as the active window, makes it activate
    // another window, which cancels that menu along with the palette.
    // Never dismiss while the right-click menu is open either.
    let menu_open = Rc::new(Cell::new(false));
    {
        let menu_open = menu_open.clone();
        window.connect_realize(move |w| {
            let Some(toplevel) = w.surface().and_downcast::<gdk::Toplevel>() else {
                return;
            };
            let win = w.clone();
            let menu_open = menu_open.clone();
            // The window starts unfocused; only a focused → unfocused change counts
            let was_focused = Cell::new(false);
            toplevel.connect_state_notify(move |t| {
                let focused = t.state().contains(gdk::ToplevelState::FOCUSED);
                if !was_focused.replace(focused) || focused || !win.is_visible() {
                    return;
                }
                let (win, t, menu_open) = (win.clone(), t.clone(), menu_open.clone());
                glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                    let focused = t.state().contains(gdk::ToplevelState::FOCUSED);
                    if !focused && win.is_visible() && !menu_open.get() {
                        win.close();
                    }
                });
            });
        });
    }

    // ── Layout ────────────────────────────────────────────────────────────────
    let main_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    main_box.add_css_class("main-box");

    let search = gtk4::SearchEntry::new();
    search.set_placeholder_text(Some("Search clipboard history…"));
    search.add_css_class("search-entry");

    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scrolled.set_vexpand(true);

    let list_box = gtk4::ListBox::new();
    list_box.add_css_class("clip-list");
    scrolled.set_child(Some(&list_box));

    let hint = gtk4::Label::new(Some("Right-click a clip for more options"));
    hint.add_css_class("footer-hint");

    main_box.append(&search);
    main_box.append(&scrolled);
    main_box.append(&hint);
    window.set_child(Some(&main_box));

    let palette = Rc::new(Palette {
        socket_path,
        window: window.clone(),
        search: search.clone(),
        scrolled,
        list_box: list_box.clone(),
        all_entries: RefCell::new(Vec::new()),
        shown: RefCell::new(Vec::new()),
        thumbs: RefCell::new(HashMap::new()),
        subscribed: Cell::new(false),
        row_menu: gtk4::Popover::new(),
        menu_open,
    });
    palette.install_row_menu();
    palette.install_hover_select();

    // The window owns the palette; every other closure holds a weak reference.
    {
        let owner = RefCell::new(Some(palette.clone()));
        window.connect_destroy(move |_| drop(owner.take()));
    }

    // ── Search (debounced to prevent flicker) ────────────────────────────────
    {
        let weak = Rc::downgrade(&palette);
        let debounce_id: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        search.connect_search_changed(move |_| {
            if let Some(id) = debounce_id.take() {
                id.remove();
            }
            let weak = weak.clone();
            let debounce = debounce_id.clone();
            let id = glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                debounce.set(None);
                if let Some(p) = weak.upgrade() {
                    p.render(false);
                }
            });
            debounce_id.set(Some(id));
        });
    }

    // ── Keyboard ──────────────────────────────────────────────────────────────
    // Capture phase: the SearchEntry has focus and would otherwise consume
    // digits, arrows and Enter before the window ever sees them.
    {
        let weak = Rc::downgrade(&palette);
        let ctrl = gtk4::EventControllerKey::new();
        ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        ctrl.connect_key_pressed(move |_, keyval, _, state| {
            let Some(p) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if p.handle_key(keyval, state) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(ctrl);
    }

    // ── Row click ─────────────────────────────────────────────────────────────
    {
        let weak = Rc::downgrade(&palette);
        list_box.connect_row_activated(move |_, row| {
            let Some(p) = weak.upgrade() else { return };
            let id = p.shown.borrow().iter().find(|(_, r)| r == row).map(|(e, _)| e.id);
            if let Some(id) = id {
                p.copy(id);
            }
        });
    }

    palette.subscribe();
    palette.reload();

    let ts = (glib::monotonic_time() / 1000) as u32;
    window.present_with_time(ts);
    search.grab_focus();
}

impl Palette {
    /// Handle a key press; returns `true` if it was consumed.
    fn handle_key(self: &Rc<Self>, keyval: gdk::Key, state: gdk::ModifierType) -> bool {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let alt = state.contains(gdk::ModifierType::ALT_MASK);
        let sup = state.contains(gdk::ModifierType::SUPER_MASK);
        let selected = || self.selected_entry().map(|e| e.id);

        match keyval {
            gdk::Key::Escape => self.window.close(),
            gdk::Key::Menu => self.open_menu_for_selection(),
            gdk::Key::F10 if state.contains(gdk::ModifierType::SHIFT_MASK) => {
                self.open_menu_for_selection()
            }
            gdk::Key::Down => self.move_selection(1),
            gdk::Key::Up => self.move_selection(-1),
            gdk::Key::Return | gdk::Key::KP_Enter => {
                if let Some(id) = selected() {
                    self.copy(id);
                }
            }
            gdk::Key::BackSpace if ctrl => self.clear_history(),
            gdk::Key::p | gdk::Key::P if alt => {
                if let Some(id) = selected() {
                    self.toggle_pin(id);
                }
            }
            gdk::Key::Delete | gdk::Key::KP_Delete | gdk::Key::BackSpace if alt => {
                if let Some(id) = selected() {
                    self.delete(id);
                }
            }
            _ => {
                // 1–9 quick-copy: plain digits only while the search box is
                // empty; Alt+digit works at any time.
                let Some(idx) = quick_copy_index(keyval) else {
                    return false;
                };
                if ctrl || sup || (!alt && !self.search.text().is_empty()) {
                    return false;
                }
                let id = self.shown.borrow().get(idx).map(|(e, _)| e.id);
                if let Some(id) = id {
                    self.copy(id);
                }
            }
        }
        true
    }

    // ── Data ──────────────────────────────────────────────────────────────────

    /// Refetch history from the daemon and redraw, keeping the selection.
    fn reload(self: &Rc<Self>) {
        *self.all_entries.borrow_mut() =
            client::list_entries(None, &self.socket_path).unwrap_or_default();
        self.render(true);
    }

    /// Listen for daemon events and reload on each, so the list stays live while
    /// the palette is open (new copies, changes from other clients).
    fn subscribe(self: &Rc<Self>) {
        let (reader, handle) = match client::subscribe(&self.socket_path) {
            Ok(sub) => sub,
            Err(e) => {
                eprintln!("[klip-gui] Live updates unavailable: {e}");
                return;
            }
        };
        self.subscribed.set(true);

        let (tx, rx) = async_channel::unbounded::<()>();
        std::thread::spawn(move || {
            for line in reader.lines() {
                if line.is_err() || tx.send_blocking(()).is_err() {
                    break;
                }
            }
        });
        // Unblock the reader thread when the window goes away
        self.window.connect_destroy(move |_| {
            let _ = handle.shutdown(std::net::Shutdown::Both);
        });

        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            while rx.recv().await.is_ok() {
                while rx.try_recv().is_ok() {} // coalesce bursts (e.g. pruning)
                let Some(p) = weak.upgrade() else { break };
                p.reload();
            }
            if let Some(p) = weak.upgrade() {
                // Daemon went away; fall back to refreshing after our own actions
                p.subscribed.set(false);
            }
        });
    }

    // ── Actions ───────────────────────────────────────────────────────────────

    fn copy(&self, id: i64) {
        if let Err(e) = client::copy_entry(id, &self.socket_path) {
            eprintln!("[klip-gui] Copy failed: {e}");
        }
        self.window.close();
    }

    fn open_link(&self, id: i64) {
        let url = self.all_entries.borrow().iter().find(|e| e.id == id).map(|e| e.content.trim().to_string());
        if let Some(url) = url {
            if let Err(e) = gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>) {
                eprintln!("[klip-gui] Could not open {url}: {e}");
            }
        }
        self.window.close();
    }

    fn open_menu_for_selection(self: &Rc<Self>) {
        if let Some(idx) = self.selected_index() {
            self.show_row_menu(idx, None);
        }
    }

    fn toggle_pin(self: &Rc<Self>, id: i64) {
        if let Err(e) = client::toggle_pin(id, &self.socket_path) {
            eprintln!("[klip-gui] Pin failed: {e}");
        }
        self.refresh_after_action();
    }

    fn delete(self: &Rc<Self>, id: i64) {
        if let Err(e) = client::delete_entry(id, &self.socket_path) {
            eprintln!("[klip-gui] Delete failed: {e}");
        }
        self.refresh_after_action();
    }

    fn clear_history(self: &Rc<Self>) {
        if let Err(e) = client::clear_history(&self.socket_path) {
            eprintln!("[klip-gui] Clear failed: {e}");
        }
        self.refresh_after_action();
    }

    fn refresh_after_action(self: &Rc<Self>) {
        // When subscribed, the daemon's event for this change triggers the reload
        if !self.subscribed.get() {
            self.reload();
        }
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    /// Rebuild the list for the current search text. With `keep_selection`,
    /// the selection stays on the same entry (or position, if it's gone);
    /// otherwise the top result is selected.
    fn render(self: &Rc<Self>, keep_selection: bool) {
        let prev = keep_selection
            .then(|| self.selected_index().map(|i| (self.shown.borrow()[i].0.id, i)))
            .flatten();

        // Remove rows by index, not `first_child`: the right-click popover is
        // also a child of the list box, and `remove` would refuse it forever
        while let Some(row) = self.list_box.row_at_index(0) {
            self.list_box.remove(&row);
        }
        self.shown.borrow_mut().clear();

        let all = self.all_entries.borrow().clone();
        if all.is_empty() {
            self.list_box.append(&placeholder_row("No clips yet — copy something!"));
            return;
        }
        let query = self.search.text();
        let results = filter_entries(all, Some(query.as_str()).filter(|q| !q.is_empty()));
        if results.is_empty() {
            self.list_box.append(&placeholder_row("No matches found."));
            return;
        }

        let mut shown = Vec::with_capacity(results.len());
        for (i, entry) in results.into_iter().enumerate() {
            let prev_pinned = shown.last().map(|(e, _): &(ClipEntry, _)| e.pinned);
            if entry.pinned && i == 0 {
                self.list_box.append(&section_label("Pinned"));
            } else if !entry.pinned && prev_pinned == Some(true) {
                self.list_box.append(&section_label("History"));
            }
            let row = self.create_row(&entry, i + 1);
            self.list_box.append(&row);
            shown.push((entry, row));
        }
        let idx = prev
            .map(|(id, i)| shown.iter().position(|(e, _)| e.id == id).unwrap_or(i))
            .unwrap_or(0);
        *self.shown.borrow_mut() = shown;
        self.select(idx);
    }

    fn selected_index(&self) -> Option<usize> {
        let row = self.list_box.selected_row()?;
        self.shown.borrow().iter().position(|(_, r)| *r == row)
    }

    fn selected_entry(&self) -> Option<ClipEntry> {
        let i = self.selected_index()?;
        Some(self.shown.borrow()[i].0.clone())
    }

    /// Select the entry at `idx` (clamped to the list) and scroll it into view.
    fn select(&self, idx: usize) {
        let shown = self.shown.borrow();
        let Some(last) = shown.len().checked_sub(1) else { return };
        let row = &shown[idx.min(last)].1;
        self.list_box.select_row(Some(row));
        if let Some(bounds) = row.compute_bounds(&self.list_box) {
            let adj = self.scrolled.vadjustment();
            let (top, bottom) = (bounds.y() as f64, (bounds.y() + bounds.height()) as f64);
            if top < adj.value() {
                adj.set_value(top);
            } else if bottom > adj.value() + adj.page_size() {
                adj.set_value(bottom - adj.page_size());
            }
        }
    }

    /// Moving the pointer over a row selects it, so the hover and keyboard
    /// highlights never disagree. Only real motion counts: rows rebuilt under
    /// a still pointer (e.g. while typing a search) keep the top result.
    fn install_hover_select(self: &Rc<Self>) {
        let motion = gtk4::EventControllerMotion::new();
        let last = Cell::new((f64::NAN, f64::NAN));
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            let Some(p) = weak.upgrade() else { return };
            if last.replace((x, y)) == (x, y) || p.menu_open.get() {
                return;
            }
            let Some(row) = p.list_box.row_at_y(y as i32) else { return };
            if row.is_selectable() && p.list_box.selected_row().as_ref() != Some(&row) {
                // Not `select`: the row is under the pointer, so don't scroll
                p.list_box.select_row(Some(&row));
            }
        });
        self.list_box.add_controller(motion);
    }

    fn move_selection(&self, delta: isize) {
        let next = match self.selected_index() {
            Some(i) => i.saturating_add_signed(delta),
            None => 0,
        };
        self.select(next);
    }

    fn thumb(&self, entry: &ClipEntry) -> Thumb {
        self.thumbs
            .borrow_mut()
            .entry(entry.content.clone())
            .or_insert_with(|| load_thumb(entry))
            .clone()
    }

    fn create_row(self: &Rc<Self>, entry: &ClipEntry, index: usize) -> gtk4::ListBoxRow {
        let row = gtk4::ListBoxRow::new();
        row.add_css_class("clip-row");

        let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        hbox.add_css_class("row-hbox");

        if index <= 9 {
            let badge = gtk4::Label::new(Some(&index.to_string()));
            badge.add_css_class("badge");
            badge.set_valign(gtk4::Align::Center);
            hbox.append(&badge);
        }

        if entry.is_image() {
            let label = match self.thumb(entry) {
                Some((texture, w, h)) => {
                    let picture = gtk4::Picture::for_paintable(&texture);
                    picture.set_can_shrink(false);
                    picture.add_css_class("thumb");
                    hbox.append(&picture);
                    gtk4::Label::new(Some(&format!("{w}×{h}")))
                }
                None => gtk4::Label::new(Some("Image (unavailable)")),
            };
            label.set_halign(gtk4::Align::Start);
            label.set_hexpand(true);
            label.add_css_class("clip-meta");
            hbox.append(&label);
        } else {
            append_text_content(&hbox, entry);
        }

        // Pin / delete buttons. Unfocusable, so typing stays in the search box.
        let weak = Rc::downgrade(self);
        let id = entry.id;
        let pin_tip = if entry.pinned { "Unpin (Alt+P)" } else { "Pin (Alt+P)" };
        let pin = row_action_button(PIN_ICONS, pin_tip);
        if entry.pinned {
            pin.add_css_class("pinned");
        }
        pin.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.toggle_pin(id);
            }
        });
        hbox.append(&pin);

        let weak = Rc::downgrade(self);
        let delete = row_action_button(DELETE_ICONS, "Delete (Alt+Delete)");
        delete.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.delete(id);
            }
        });
        hbox.append(&delete);

        row.set_child(Some(&hbox));
        row
    }
}

/// Fuzzy-filter `entries` by `query`, pinned first, best matches next, then
/// most recent; at most [`MAX_RESULTS`].
fn filter_entries(entries: Vec<ClipEntry>, query: Option<&str>) -> Vec<ClipEntry> {
    let mut scored: Vec<(u32, ClipEntry)> = if let Some(q) = query {
        let mut matcher = nucleo::Matcher::new(nucleo::Config::DEFAULT);
        let pattern = nucleo::pattern::Pattern::new(
            q,
            nucleo::pattern::CaseMatching::Smart,
            nucleo::pattern::Normalization::Smart,
            nucleo::pattern::AtomKind::Fuzzy,
        );
        entries
            .into_iter()
            .filter_map(|e| {
                // Images have no text; let "image" / "png" find them
                let text = if e.is_image() {
                    format!("image {}", e.mime_type)
                } else {
                    e.content.clone()
                };
                let buf = nucleo::Utf32String::from(text.as_str());
                pattern.score(buf.slice(..), &mut matcher).map(|s| (s, e))
            })
            .collect()
    } else {
        entries.into_iter().map(|e| (0, e)).collect()
    };

    scored.sort_by(|a, b| {
        b.1.pinned
            .cmp(&a.1.pinned)
            .then_with(|| b.0.cmp(&a.0))
            .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
    });
    scored.into_iter().map(|(_, e)| e).take(MAX_RESULTS).collect()
}

fn load_thumb(entry: &ClipEntry) -> Thumb {
    use gtk4::gdk_pixbuf::Pixbuf;
    let path = entry.image_path()?;
    let (_, w, h) = Pixbuf::file_info(&path)?;
    // Decode straight to thumbnail size; never upscale small images
    let pixbuf = Pixbuf::from_file_at_scale(&path, THUMB_W.min(w), THUMB_H.min(h), true).ok()?;
    Some((gdk::Texture::for_pixbuf(&pixbuf), w, h))
}

/// Map `1`–`9` (main row or keypad) to a 0-based entry index.
fn quick_copy_index(keyval: gdk::Key) -> Option<usize> {
    let v = keyval.into_glib();
    [(gdk::Key::_1, gdk::Key::_9), (gdk::Key::KP_1, gdk::Key::KP_9)]
        .into_iter()
        .find_map(|(lo, hi)| {
            let (lo, hi) = (lo.into_glib(), hi.into_glib());
            (lo..=hi).contains(&v).then(|| (v - lo) as usize)
        })
}

/// Truncate to at most `max` characters (not bytes — slicing bytes panics on
/// multi-byte UTF-8 such as emoji or Devanagari).
fn truncate_chars(s: &str, max: usize) -> Option<&str> {
    s.char_indices().nth(max).map(|(i, _)| &s[..i])
}

fn section_label(text: &str) -> gtk4::ListBoxRow {
    let lbl = gtk4::Label::new(Some(text));
    lbl.add_css_class("section-header");
    lbl.set_halign(gtk4::Align::Start);
    lbl.set_margin_start(12);
    lbl.set_margin_top(8);
    lbl.set_margin_bottom(4);
    
    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&lbl));
    row.set_selectable(false);
    row.set_activatable(false);
    row.add_css_class("transparent-row");
    row
}

fn placeholder_row(text: &str) -> gtk4::ListBoxRow {
    let lbl = gtk4::Label::new(Some(text));
    lbl.add_css_class("empty-label");
    lbl.set_margin_top(24);
    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&lbl));
    row.set_selectable(false);
    row.set_activatable(false);
    row.add_css_class("transparent-row");
    row
}

fn row_action_button(icon_names: &[&str], tooltip: &str) -> gtk4::Button {
    let icon = gtk4::Image::from_gicon(&gio::ThemedIcon::from_names(icon_names));
    let button = gtk4::Button::new();
    button.set_child(Some(&icon));
    button.set_tooltip_text(Some(tooltip));
    button.set_focusable(false);
    button.set_focus_on_click(false);
    button.set_valign(gtk4::Align::Center);
    button.add_css_class("flat");
    button.add_css_class("row-action");
    button
}

/// Type icon plus the first line of a text entry, with the full text as tooltip.
fn append_text_content(hbox: &gtk4::Box, entry: &ClipEntry) {
    let type_icon_str = match entry.mime_type.as_str() {
        t if t.contains("uri")   => "🔗",
        t if t.contains("email") => "✉",
        t if t.contains("code")  => "</>",
        t if t.contains("path")  => "📁",
        t if t.contains("color") => "⬛",
        _                        => "  ",
    };
    if type_icon_str != "  " {
        let t_icon = gtk4::Label::new(Some(type_icon_str));
        t_icon.add_css_class("type-icon");
        hbox.append(&t_icon);
    }

    // Show first line only, truncated
    let first_line = entry.content.lines().next().unwrap_or("");
    let content = match truncate_chars(first_line, 120) {
        Some(head) => format!("{head}…"),
        None => first_line.to_string(),
    };
    let label = gtk4::Label::new(Some(&content));
    label.set_halign(gtk4::Align::Start);
    label.set_hexpand(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_max_width_chars(35);
    label.add_css_class("clip-content");

    if entry.mime_type.contains("code") {
        label.add_css_class("code");
    } else if entry.mime_type.contains("uri") {
        label.add_css_class("url");
    }

    label.set_has_tooltip(true);
    let full_content = entry.content.clone();
    label.connect_query_tooltip(move |_, _, _, _, tooltip| {
        let text = match truncate_chars(&full_content, 1000) {
            Some(head) => format!("{head}...\n(truncated)"),
            None => full_content.clone(),
        };
        tooltip.set_text(Some(&text));
        true
    });

    hbox.append(&label);
}
