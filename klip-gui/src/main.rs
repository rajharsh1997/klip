mod client;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use glib::translate::IntoGlib;
use klip_common::ClipEntry;
use std::path::PathBuf;
use std::rc::Rc;
use ksni::{Tray, MenuItem, menu::StandardItem, blocking::TrayMethods};

enum TrayMsg {
    Toggle,
    Quit,
}

struct KlipTray {
    tx: async_channel::Sender<TrayMsg>,
}

impl Tray for KlipTray {
    fn id(&self) -> String {
        "klip".into()
    }
    fn icon_name(&self) -> String {
        "klip".into()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Klip".into(),
            description: "Clipboard Manager".into(),
            icon_name: "klip".into(),
            icon_pixmap: vec![],
        }
    }
    fn title(&self) -> String {
        "Klip".into()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayMsg::Toggle);
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Toggle Window".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.try_send(TrayMsg::Toggle);
                }),
                ..Default::default()
            }.into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.try_send(TrayMsg::Quit);
                }),
                ..Default::default()
            }.into(),
        ]
    }
}

fn default_socket_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("klip").join("klip.sock")
}

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
    let app = gtk4::Application::new(
        Some("com.klip.clipboard-manager"),
        gio::ApplicationFlags::empty(),
    );

    // `startup` only fires in the primary instance. A second `klip` invocation
    // (e.g. from the global hotkey) just forwards `activate` over D-Bus and exits,
    // so creating the tray here avoids a temporary duplicate tray icon.
    app.connect_startup(|app| {
        std::mem::forget(app.hold()); // Keeps the GTK application alive in the background even when 0 windows exist
        spawn_tray(app);
    });

    app.connect_activate(|app| {
        let socket_path = default_socket_path();
        ensure_daemon_running(&socket_path);
        if let Some(win) = app.active_window() {
            let ts = (glib::monotonic_time() / 1000) as u32;
            win.present_with_time(ts);
        } else {
            build_ui(app, socket_path);
        }
    });

    app.run()
}

fn spawn_tray(app: &gtk4::Application) {
    let (tx, rx) = async_channel::unbounded();
    let tray = KlipTray { tx };
    match tray.spawn() {
        Ok(handle) => std::mem::forget(handle),
        Err(e) => {
            // No StatusNotifier host (e.g. stock GNOME without the AppIndicator
            // extension) — the palette still works via the hotkey.
            eprintln!("[klip-gui] Tray icon unavailable: {e}");
            return;
        }
    }

    let app_clone = app.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(msg) = rx.recv().await {
            match msg {
                TrayMsg::Toggle => {
                    if let Some(win) = app_clone.active_window() {
                        win.close();
                    } else {
                        app_clone.activate();
                    }
                }
                TrayMsg::Quit => {
                    app_clone.quit();
                    std::process::exit(0);
                }
            }
        }
    });
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

    // Auto-dismiss on focus loss with a small debounce to ignore compositor mapping glitches
    window.connect_is_active_notify(move |w| {
        if !w.is_active() && w.is_visible() {
            let win = w.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if !win.is_active() && win.is_visible() {
                    win.close();
                }
            });
        }
    });

    // Removed Wayland layer-shell initialization because it causes focus-stealing bugs
    // The window will now behave as a standard GTK floating window centered on the screen.

    // ── CSS ───────────────────────────────────────────────────────────────────
    let css = gtk4::CssProvider::new();
    css.load_from_data(include_str!("style.css"));
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("No display"),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // ── Layout ────────────────────────────────────────────────────────────────
    let main_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    main_box.add_css_class("main-box");

    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_placeholder_text(Some("Search clipboard history…"));
    search_entry.add_css_class("search-entry");

    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scrolled.set_vexpand(true);

    let list_box = gtk4::ListBox::new();
    list_box.add_css_class("clip-list");
    scrolled.set_child(Some(&list_box));

    main_box.append(&search_entry);
    main_box.append(&scrolled);
    window.set_child(Some(&main_box));

    // ── State ─────────────────────────────────────────────────────────────────
    let initial_entries = client::list_entries(None, &socket_path).unwrap_or_default();
    let all_entries: Rc<std::cell::RefCell<Vec<ClipEntry>>> =
        Rc::new(std::cell::RefCell::new(initial_entries.clone()));
    let entries: Rc<std::cell::RefCell<Vec<ClipEntry>>> =
        Rc::new(std::cell::RefCell::new(initial_entries));

    // ── Refresh helper ────────────────────────────────────────────────────────
    fn refresh_list(
        query: Option<&str>,
        all_entries: &Rc<std::cell::RefCell<Vec<ClipEntry>>>,
        entries: &Rc<std::cell::RefCell<Vec<ClipEntry>>>,
        list_box: &gtk4::ListBox,
    ) {
        while let Some(child) = list_box.first_child() {
            list_box.remove(&child);
        }
        
        let fetched = all_entries.borrow().clone();
        if fetched.is_empty() {
            let lbl = gtk4::Label::new(Some("No clips yet — copy something!"));
            lbl.add_css_class("empty-label");
            lbl.set_margin_top(24);
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&lbl));
            row.set_selectable(false);
            row.set_activatable(false);
            row.add_css_class("transparent-row");
            list_box.append(&row);
            return;
        }

        let mut scored: Vec<(u32, ClipEntry)> = if let Some(q) = query.filter(|s| !s.is_empty()) {
            let mut matcher = nucleo::Matcher::new(nucleo::Config::DEFAULT);
            let pattern = nucleo::pattern::Pattern::new(
                q,
                nucleo::pattern::CaseMatching::Smart,
                nucleo::pattern::Normalization::Smart,
                nucleo::pattern::AtomKind::Fuzzy,
            );
            fetched.into_iter()
                .filter_map(|e| {
                    let buf = nucleo::Utf32String::from(e.content.as_str());
                    pattern.score(buf.slice(..), &mut matcher).map(|s| (s, e))
                })
                .collect()
        } else {
            fetched.into_iter().map(|e| (0, e)).collect()
        };

        scored.sort_by(|a, b| {
            b.1.pinned.cmp(&a.1.pinned)
                .then_with(|| b.0.cmp(&a.0))
                .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
        });
        
        // LIMIT TO 50 ITEMS to save memory and layout time!
        let results: Vec<ClipEntry> = scored.into_iter().map(|(_, e)| e).take(50).collect();
        *entries.borrow_mut() = results.clone();
        
        let has_pinned = results.iter().any(|e| e.pinned);
        let has_history = results.iter().any(|e| !e.pinned);
        
        if has_pinned {
            list_box.append(&section_label("Pinned"));
        }
        
        for (i, entry) in results.iter().enumerate() {
            if !entry.pinned && i > 0 && results[i - 1].pinned {
                list_box.append(&section_label("History"));
            }
            list_box.append(&create_entry_row(entry, i + 1));
        }
        
        if !has_pinned && !has_history && query.is_some() {
            let lbl = gtk4::Label::new(Some("No matches found."));
            lbl.add_css_class("empty-label");
            lbl.set_margin_top(24);
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&lbl));
            row.set_selectable(false);
            row.set_activatable(false);
            row.add_css_class("transparent-row");
            list_box.append(&row);
        }
    }

    // ── Search (debounced to prevent flicker) ────────────────────────────────
    {
        let all_entries = all_entries.clone();
        let entries = entries.clone();
        let list_box = list_box.clone();
        let debounce_id: Rc<std::cell::Cell<Option<glib::SourceId>>> = Rc::new(std::cell::Cell::new(None));
        search_entry.connect_search_changed(move |e| {
            if let Some(id) = debounce_id.take() {
                id.remove();
            }
            let q = e.text();
            let q = if q.is_empty() { None } else { Some(q.to_string()) };
            
            let all_entries = all_entries.clone();
            let entries = entries.clone();
            let list_box = list_box.clone();
            let debounce = debounce_id.clone();
            
            let id = glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                refresh_list(q.as_deref(), &all_entries, &entries, &list_box);
                debounce.set(None);
            });
            debounce_id.set(Some(id));
        });
    }

    // ── Keyboard: Escape / Ctrl+Backspace (bubble, after SearchEntry) ─────────
    {
        let window_esc = window.clone();
        let socket_path = socket_path.clone();
        let all_entries = all_entries.clone();
        let entries = entries.clone();
        let list_box = list_box.clone();
        let search = search_entry.clone();
        let ctrl = gtk4::EventControllerKey::new();
        ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        ctrl.connect_key_pressed(move |_, keyval, _, state| {
            if keyval == gdk::Key::Escape {
                window_esc.close();
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::BackSpace
                && state.contains(gdk::ModifierType::CONTROL_MASK)
            {
                let _ = client::clear_history(&socket_path);
                *all_entries.borrow_mut() = client::list_entries(None, &socket_path).unwrap_or_default();
                let q = search.text();
                refresh_list(Some(q.as_str()).filter(|s| !s.is_empty()), &all_entries, &entries, &list_box);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(ctrl);
    }

    // ── Keyboard: 1-9 quick-copy ──────────────────────────────────────────────
    // Capture phase: the SearchEntry has focus and would otherwise consume the
    // digit as search text before the window ever sees it. Plain digits act only
    // while the search box is empty; Alt+digit works at any time.
    {
        let entries = entries.clone();
        let socket_path = socket_path.clone();
        let window_digit = window.clone();
        let search = search_entry.clone();
        let ctrl = gtk4::EventControllerKey::new();
        ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        ctrl.connect_key_pressed(move |_, keyval, _, state| {
            let Some(idx) = quick_copy_index(keyval) else {
                return glib::Propagation::Proceed;
            };
            let alt = state.contains(gdk::ModifierType::ALT_MASK);
            let other_mods = state.intersects(
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SUPER_MASK,
            );
            if other_mods || (!alt && !search.text().is_empty()) {
                return glib::Propagation::Proceed;
            }
            let ents = entries.borrow();
            if let Some(entry) = ents.get(idx) {
                let _ = client::copy_entry(entry.id, &socket_path);
                window_digit.close();
            }
            glib::Propagation::Stop
        });
        window.add_controller(ctrl);
    }

    // ── Row click ─────────────────────────────────────────────────────────────
    {
        let entries = entries.clone();
        let socket_path = socket_path.clone();
        let window = window.clone();
        list_box.connect_row_activated(move |_, row| {
            let ents = entries.borrow();
            if let Ok(id) = row.widget_name().parse::<i64>() {
                if let Some(entry) = ents.iter().find(|e| e.id == id) {
                    let _ = client::copy_entry(entry.id, &socket_path);
                    window.close();
                }
            }
        });
    }

    // ── Show & raise ──────────────────────────────────────────────────────────
    let all_entries_ref = all_entries.clone();
    let entries_ref = entries.clone();
    let list_ref = list_box.clone();
    refresh_list(None, &all_entries_ref, &entries_ref, &list_ref);

    let ts = (glib::monotonic_time() / 1000) as u32;
    window.present_with_time(ts);
    search_entry.grab_focus();
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

fn create_entry_row(entry: &ClipEntry, index: usize) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.set_widget_name(&entry.id.to_string());
    row.add_css_class("clip-row");

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    hbox.add_css_class("row-hbox");

    if entry.pinned {
        let icon = gtk4::Image::from_icon_name("pin-symbolic");
        icon.add_css_class("pin-icon");
        hbox.append(&icon);
    }

    if index <= 9 {
        let badge = gtk4::Label::new(Some(&index.to_string()));
        badge.add_css_class("badge");
        hbox.append(&badge);
    }

    let type_icon_str = match entry.mime_type.as_str() {
        t if t.contains("url")   => "🔗",
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
    } else if entry.mime_type.contains("url") {
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
    row.set_child(Some(&hbox));
    row
}
