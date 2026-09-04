mod client;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use glib::translate::IntoGlib;
use klip_common::ClipEntry;
use std::path::PathBuf;
use std::rc::Rc;

fn default_socket_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("klip").join("klip.sock")
}

/// Try to start the daemon if it's not already running.
fn ensure_daemon_running(socket_path: &PathBuf) {
    if socket_path.exists() {
        if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
            return;
        }
        eprintln!("[klip-gui] Found stale socket, removing...");
        let _ = std::fs::remove_file(socket_path);
    }

    eprintln!("[klip-gui] Daemon socket not found, starting daemon...");

    // Try systemd first (preferred — handles lifecycle, auto-restart, etc.)
    let systemd_ok = std::process::Command::new("systemctl")
        .args(["--user", "start", "klipd"])
        .status()
        .ok()
        .map(|s| s.success())
        .unwrap_or(false);

    if systemd_ok {
        for _ in 0..20 {
            if socket_path.exists() {
                eprintln!("[klip-gui] Daemon started via systemd");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        eprintln!("[klip-gui] systemd start returned ok but socket not yet visible, proceeding...");
        return;
    }

    // Fallback: spawn daemon directly (non-systemd: static distros, containers, etc.)
    eprintln!("[klip-gui] Falling back to direct daemon spawn...");
    match std::process::Command::new("klipd")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {
            for _ in 0..20 {
                if socket_path.exists() {
                    eprintln!("[klip-gui] Daemon started directly");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            eprintln!("[klip-gui] Daemon process spawned but socket not yet visible, proceeding...");
        }
        Err(e) => {
            eprintln!("[klip-gui] Could not start daemon: {e}");
        }
    }
}

fn main() -> glib::ExitCode {
    let app = gtk4::Application::new(
        Some("com.klip.clipboard-manager"),
        gio::ApplicationFlags::empty(),
    );

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

fn build_ui(app: &gtk4::Application, socket_path: PathBuf) {
    // ── Window ────────────────────────────────────────────────────────────────
    let window = gtk4::ApplicationWindow::new(app);
    window.set_title(Some("Klip"));
    window.set_default_size(320, 480);
    window.set_resizable(false);
    window.set_decorated(false);
    window.add_css_class("klip-popup");
    window.set_icon_name(Some("klip"));

    // Hide instead of quit when the window is closed
    window.connect_close_request(move |w| {
        w.hide();
        glib::Propagation::Stop
    });

    // Auto-dismiss on focus loss with a small debounce to ignore compositor mapping glitches
    window.connect_is_active_notify(move |w| {
        if !w.is_active() && w.is_visible() {
            let win = w.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if !win.is_active() && win.is_visible() {
                    win.hide();
                }
            });
        }
    });

    // Wayland positioning
    if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("XDG_SESSION_TYPE").map(|s| s == "wayland").unwrap_or(false) {
        use gtk4_layer_shell::{Layer, Edge, LayerShell, KeyboardMode};
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Top, true);
        window.set_margin(Edge::Top, 8);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
    }

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
    let entries: Rc<std::cell::RefCell<Vec<ClipEntry>>> =
        Rc::new(std::cell::RefCell::new(Vec::new()));

    // ── Refresh helper ────────────────────────────────────────────────────────
    fn refresh_list(
        query: Option<&str>,
        entries: &Rc<std::cell::RefCell<Vec<ClipEntry>>>,
        list_box: &gtk4::ListBox,
        socket_path: &PathBuf,
    ) {
        while let Some(child) = list_box.first_child() {
            list_box.remove(&child);
        }
        match client::list_entries(None, socket_path) {
            Ok(fetched) => {
                let mut scored: Vec<(u32, ClipEntry)> = if let Some(q) = query.filter(|s| !s.is_empty()) {
                    let mut matcher = nucleo::Matcher::new(nucleo::Config::DEFAULT);
                    let mut pattern = nucleo::pattern::Pattern::new(
                        q,
                        nucleo::pattern::CaseMatching::Smart,
                        nucleo::pattern::Normalization::Smart,
                        nucleo::pattern::AtomKind::Fuzzy,
                    );
                    fetched.into_iter()
                        .filter_map(|e| {
                            let mut buf = nucleo::Utf32String::from(e.content.as_str());
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
                
                let fetched: Vec<ClipEntry> = scored.into_iter().map(|(_, e)| e).collect();
                *entries.borrow_mut() = fetched.clone();
                let has_pinned = fetched.iter().any(|e| e.pinned);
                let has_history = fetched.iter().any(|e| !e.pinned);
                if has_pinned {
                    list_box.append(&section_label("Pinned"));
                }
                for (i, entry) in fetched.iter().enumerate() {
                    if !entry.pinned && i > 0 && fetched[i - 1].pinned {
                        list_box.append(&section_label("History"));
                    }
                    list_box.append(&create_entry_row(entry, i + 1));
                }
                if !has_pinned && !has_history {
                    let lbl = gtk4::Label::new(Some("No clips yet — copy something!"));
                    lbl.add_css_class("empty-label");
                    lbl.set_margin_top(24);
                    list_box.append(&lbl);
                }
            }
            Err(e) => {
                let lbl = gtk4::Label::new(Some(&format!("Cannot reach klip daemon: {e}")));
                lbl.add_css_class("error-label");
                lbl.set_margin_top(24);
                list_box.append(&lbl);
            }
        }
    }

    // ── Search (debounced to prevent flicker) ────────────────────────────────
    {
        let entries = entries.clone();
        let list_box = list_box.clone();
        let socket_path = socket_path.clone();
        let debounce_id: Rc<std::cell::Cell<Option<glib::SourceId>>> = Rc::new(std::cell::Cell::new(None));
        search_entry.connect_search_changed(move |e| {
            // Cancel any pending refresh
            if let Some(id) = debounce_id.take() {
                id.remove();
            }
            // Capture the query text now (before the timeout)
            let q = e.text();
            let q = if q.is_empty() { None } else { Some(q.to_string()) };
            // Schedule a refresh after 150ms debounce
            let entries = entries.clone();
            let list_box = list_box.clone();
            let socket_path = socket_path.clone();
            let debounce = debounce_id.clone();
            let id = glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                refresh_list(q.as_deref(), &entries, &list_box, &socket_path);
                debounce.set(None);
            });
            debounce_id.set(Some(id));
        });
    }

    // ── Keyboard: Escape / Ctrl+Backspace (bubble, after SearchEntry) ─────────
    {
        let window_esc = window.clone();
        let socket_path = socket_path.clone();
        let ctrl = gtk4::EventControllerKey::new();
        ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        ctrl.connect_key_pressed(move |_, keyval, _, state| {
            if keyval == gdk::Key::Escape {
                window_esc.hide();
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::BackSpace
                && state.contains(gdk::ModifierType::CONTROL_MASK)
            {
                let _ = client::clear_history(&socket_path);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(ctrl);
    }

    // ── Keyboard: digits 1-9 quick-copy (capture on window, only when search is empty) ─
    {
        let entries = entries.clone();
        let socket_path = socket_path.clone();
        let window_digit = window.clone();
        let search = search_entry.clone();
        let ctrl = gtk4::EventControllerKey::new();
        ctrl.connect_key_pressed(move |_, keyval, _, _| {
            // Only intercept digits when search box is empty (not typing a search)
            if !search.text().is_empty() {
                return glib::Propagation::Proceed;
            }
            let v = keyval.into_glib();
            let lo = gdk::Key::_1.into_glib();
            let hi = gdk::Key::_9.into_glib();
            if (lo..=hi).contains(&v) {
                let idx = (v - lo) as usize;
                let ents = entries.borrow();
                if idx < ents.len() {
                    let _ = client::copy_entry(ents[idx].id, &socket_path);
                    window_digit.hide();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
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
                    window.hide();
                }
            }
        });
    }

    // ── Show & raise ──────────────────────────────────────────────────────────
    // present_with_time with a current monotonic timestamp tells KDE to raise
    // and focus the window, bypassing focus-stealing prevention.
    let ts = (glib::monotonic_time() / 1000) as u32;
    window.present_with_time(ts);

    // Grab keyboard focus once the window is actually on screen
    let se = search_entry.clone();
    let entries_map = entries.clone();
    let list_map = list_box.clone();
    let sp_map = socket_path.clone();
    window.connect_map(move |_| {
        se.set_text(""); 
        refresh_list(None, &entries_map, &list_map, &sp_map);
        se.grab_focus();
    });
}

fn section_label(text: &str) -> gtk4::Label {
    let lbl = gtk4::Label::new(Some(text));
    lbl.add_css_class("section-header");
    lbl.set_halign(gtk4::Align::Start);
    lbl.set_margin_start(12);
    lbl.set_margin_top(8);
    lbl.set_margin_bottom(4);
    lbl
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
    let content = entry.content.lines().next().unwrap_or("").to_string();
    let content = if content.len() > 120 {
        format!("{}…", &content[..120])
    } else {
        content
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
        let text = if full_content.len() > 1000 {
            format!("{}...\n(truncated)", &full_content[..1000])
        } else {
            full_content.clone()
        };
        tooltip.set_text(Some(&text));
        true
    });

    hbox.append(&label);
    row.set_child(Some(&hbox));
    row
}
