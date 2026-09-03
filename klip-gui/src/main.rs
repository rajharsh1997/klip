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
    // KDE Plasma Wayland: GTK4 native Wayland backend doesn't receive an XDG
    // activation token when launched from a shortcut/terminal, so the compositor
    // never raises the window. Force XWayland which works correctly on KDE.
    if std::env::var("GDK_BACKEND").is_err() {
        std::env::set_var("GDK_BACKEND", "x11");
    }

    let app = gtk4::Application::new(
        Some("com.klip.clipboard-manager"),
        gio::ApplicationFlags::NON_UNIQUE,
    );

    app.connect_activate(|app| {
        let socket_path = default_socket_path();
        ensure_daemon_running(&socket_path);
        build_ui(app, socket_path);
    });

    app.run()
}

fn build_ui(app: &gtk4::Application, socket_path: PathBuf) {
    // ── Window ────────────────────────────────────────────────────────────────
    let window = gtk4::ApplicationWindow::new(app);
    window.set_title(Some("Klip"));
    window.set_default_size(460, 520);
    window.set_resizable(true);
    window.set_decorated(true);
    window.set_icon_name(Some("klip"));

    // Quit the app loop when the window is closed
    let app_for_close = app.clone();
    window.connect_close_request(move |_| {
        app_for_close.quit();
        glib::Propagation::Proceed
    });

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
        match client::list_entries(query, socket_path) {
            Ok(mut fetched) => {
                fetched.sort_by(|a, b| {
                    b.pinned.cmp(&a.pinned)
                        .then_with(|| b.updated_at.cmp(&a.updated_at))
                });
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

    refresh_list(None, &entries, &list_box, &socket_path);

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
        ctrl.set_propagation_phase(gtk4::PropagationPhase::Bubble);
        ctrl.connect_key_pressed(move |_, keyval, _, state| {
            if keyval == gdk::Key::Escape {
                window_esc.close();
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
                    window_digit.close();
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
                    window.close();
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
    window.connect_map(move |_| {
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

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    hbox.set_margin_start(12);
    hbox.set_margin_end(12);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);

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
    label.set_max_width_chars(50);
    label.add_css_class("clip-content");
    hbox.append(&label);

    row.set_child(Some(&hbox));
    row
}
