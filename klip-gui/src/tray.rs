//! System-tray icon (StatusNotifierItem via ksni).
//!
//! The menu lists recent clips (click to copy) and offers pause, clear and
//! autostart. A background thread subscribes to daemon events and
//! refreshes the menu, so it stays current without the palette being open.

use crate::client;
use gtk4::glib;
use gtk4::prelude::*;
use klip_common::ClipEntry;
use ksni::blocking::TrayMethods;
use ksni::menu::{StandardItem, SubMenu};
use ksni::{MenuItem, Tray};
use std::io::BufRead;
use std::path::PathBuf;
use std::time::Duration;

/// Clips listed in the menu, and how many of those may be pinned ones (so
/// recent clips always get some room).
const MENU_CLIPS: usize = 5;
const MENU_PINNED: usize = 3;
/// Menu labels are cut to this many characters.
const LABEL_CHARS: usize = 45;

/// Requests that must run on the GTK main thread.
enum TrayMsg {
    Toggle,
    Quit,
}

/// Daemon state shown in the tray.
struct Status {
    /// The clips listed in the menu (pinned first).
    entries: Vec<MenuClip>,
    count: usize,
    paused: bool,
}

/// What kind of clip a menu row shows, which picks its theme icon.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Text,
    Link,
    Code,
    Email,
    Path,
    Color,
    Image,
}

impl Kind {
    fn of(entry: &ClipEntry) -> Self {
        match entry.mime_type.as_str() {
            m if m.starts_with("image/") => Kind::Image,
            "text/uri-list" => Kind::Link,
            "text/x-code" => Kind::Code,
            "text/x-email" => Kind::Email,
            "text/x-path" => Kind::Path,
            "text/x-color" => Kind::Color,
            _ => Kind::Text,
        }
    }
}

/// A clip as shown in the tray menu.
struct MenuClip {
    id: i64,
    pinned: bool,
    kind: Kind,
    label: String,
    /// Own picture instead of a theme icon: an image's thumbnail or a colour's
    /// swatch, PNG-encoded.
    picture: Option<Vec<u8>>,
}

/// Menu icon names, resolved against the current icon theme at startup: KDE's
/// Breeze has e.g. `pin` and `edit-clear-history`, while GNOME/Ubuntu's
/// Adwaita only ships `-symbolic` variants of most action icons.
struct Icons {
    pinned: String,
    text: String,
    link: String,
    code: String,
    email: String,
    path: String,
    image: String,
    open: String,
    pause: String,
    resume: String,
    clear: String,
    delete: String,
    autostart: String,
    quit: String,
    info: String,
}

impl Icons {
    /// Must run on the GTK main thread.
    fn resolve() -> Self {
        let theme = gtk4::gdk::Display::default().map(|d| gtk4::IconTheme::for_display(&d));
        let pick = |names: &[&str]| -> String {
            let found = theme.as_ref().and_then(|t| names.iter().find(|n| t.has_icon(n)));
            // Unknown theme: the first (Breeze/freedesktop) name is the best guess
            found.unwrap_or(&names[0]).to_string()
        };
        Self {
            pinned: pick(&["pin", "view-pin-symbolic", "pin-symbolic", "starred-symbolic"]),
            // Monochrome action-style icons, to match the rest of the menu
            text: pick(&["edit-paste", "edit-paste-symbolic"]),
            link: pick(&["insert-link", "insert-link-symbolic"]),
            code: pick(&["code-context", "utilities-terminal-symbolic", "text-x-script"]),
            email: pick(&["mail-message", "mail-unread-symbolic"]),
            path: pick(&["document-open", "document-open-symbolic", "folder-symbolic"]),
            image: pick(&["image-x-generic", "image-x-generic-symbolic"]),
            open: pick(&["klip", "edit-paste", "edit-paste-symbolic"]),
            pause: pick(&["media-playback-pause", "media-playback-pause-symbolic"]),
            resume: pick(&["media-playback-start", "media-playback-start-symbolic"]),
            clear: pick(&["edit-clear-history", "edit-clear-all-symbolic", "edit-clear-symbolic"]),
            delete: pick(&["edit-delete", "edit-delete-symbolic", "user-trash-symbolic"]),
            autostart: pick(&["system-run", "system-run-symbolic"]),
            quit: pick(&["application-exit", "application-exit-symbolic", "system-shutdown-symbolic"]),
            info: pick(&["dialog-information", "dialog-information-symbolic"]),
        }
    }
}

struct KlipTray {
    tx: async_channel::Sender<TrayMsg>,
    icons: Icons,
    socket_path: PathBuf,
    /// `None` while the daemon is unreachable.
    status: Option<Status>,
    autostart: bool,
    /// Flipped whenever the clip rows change; it adds or drops a hidden item at
    /// the end of the menu. ksni numbers items by position and sends only
    /// property diffs unless the layout changes, and Plasma mishandles some
    /// diffs: when a row goes from a thumbnail (icon-data) to a theme icon it
    /// sets the new icon-name, then applies "icon-data removed" by clearing the
    /// icon, so the row ends up blank; a separator moving rows isn't applied at
    /// all. A layout change makes ksni hand out fresh ids, so the panel builds
    /// every row from scratch.
    relayout: bool,
}

impl KlipTray {
    fn set_paused(&mut self, paused: bool) {
        match client::set_paused(paused, &self.socket_path) {
            // Update right away; the daemon's event confirms it shortly after
            Ok(()) => {
                if let Some(st) = &mut self.status {
                    st.paused = paused;
                }
            }
            Err(e) => eprintln!("[klip-gui] Pause failed: {e}"),
        }
    }

    fn set_autostart(&mut self, enable: bool) {
        match set_autostart(enable) {
            Ok(()) => self.autostart = enable,
            Err(e) => eprintln!("[klip-gui] Autostart change failed: {e}"),
        }
    }
}

impl Tray for KlipTray {
    fn id(&self) -> String {
        "klip".into()
    }
    fn icon_name(&self) -> String {
        "klip".into()
    }
    fn overlay_icon_name(&self) -> String {
        match &self.status {
            Some(st) if st.paused => "media-playback-pause".into(),
            _ => String::new(),
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match &self.status {
            None => "Daemon not running".into(),
            Some(st) if st.paused => format!("Capture paused · {}", clips_text(st.count)),
            Some(st) => clips_text(st.count),
        };
        ksni::ToolTip {
            title: "Klip".into(),
            description,
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
        // Every item gets an icon and none is a checkbox: the panel reserves an
        // icon (and checkbox) column for all rows as soon as one row uses it,
        // so a mix leaves rows with a wide empty gap before their text.
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        // ── Recent clips ──────────────────────────────────────────────────────
        match &self.status {
            None => items.push(self.disabled_item("Daemon not running")),
            Some(st) if st.entries.is_empty() => items.push(self.disabled_item("No clips yet")),
            Some(st) => {
                for (i, clip) in st.entries.iter().enumerate() {
                    if i > 0 && st.entries[i - 1].pinned && !clip.pinned {
                        items.push(MenuItem::Separator);
                    }
                    let id = clip.id;
                    // A thumbnail/swatch identifies a clip better than any icon
                    let (icon_name, icon_data) = match &clip.picture {
                        Some(png) => (String::new(), png.clone()),
                        None => (self.clip_icon(clip).to_string(), Vec::new()),
                    };
                    items.push(
                        StandardItem {
                            label: clip.label.clone(),
                            icon_name,
                            icon_data,
                            activate: Box::new(move |this: &mut Self| {
                                if let Err(e) = client::copy_entry(id, &this.socket_path) {
                                    eprintln!("[klip-gui] Copy failed: {e}");
                                }
                            }),
                            ..Default::default()
                        }
                        .into(),
                    );
                }
            }
        }
        items.push(MenuItem::Separator);

        // ── Actions ───────────────────────────────────────────────────────────
        items.push(
            StandardItem {
                label: "Open Klip".into(),
                icon_name: self.icons.open.clone(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.try_send(TrayMsg::Toggle);
                }),
                ..Default::default()
            }
            .into(),
        );
        if let Some(st) = &self.status {
            let paused = st.paused;
            items.push(
                StandardItem {
                    label: if paused { "Resume Capture" } else { "Pause Capture" }.into(),
                    icon_name: if paused { &self.icons.resume } else { &self.icons.pause }.clone(),
                    activate: Box::new(move |this: &mut Self| this.set_paused(!paused)),
                    ..Default::default()
                }
                .into(),
            );
            // A submenu, so a stray click can't wipe history
            items.push(
                SubMenu {
                    label: "Clear History".into(),
                    icon_name: self.icons.clear.clone(),
                    enabled: st.count > 0,
                    submenu: vec![StandardItem {
                        label: "Delete all unpinned clips".into(),
                        icon_name: self.icons.delete.clone(),
                        activate: Box::new(|this: &mut Self| {
                            if let Err(e) = client::clear_history(&this.socket_path) {
                                eprintln!("[klip-gui] Clear failed: {e}");
                            }
                        }),
                        ..Default::default()
                    }
                    .into()],
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);

        // ── Preferences ───────────────────────────────────────────────────────
        let autostart = self.autostart;
        items.push(
            StandardItem {
                label: if autostart { "Start at Login: On" } else { "Start at Login: Off" }.into(),
                icon_name: self.icons.autostart.clone(),
                activate: Box::new(move |this: &mut Self| this.set_autostart(!autostart)),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: self.icons.quit.clone(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.try_send(TrayMsg::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        if self.relayout {
            items.push(StandardItem { visible: false, ..Default::default() }.into());
        }
        items
    }
}

pub fn spawn_tray(app: &gtk4::Application) {
    let (tx, rx) = async_channel::unbounded();
    let socket_path = klip_common::socket_path();
    let tray = KlipTray {
        tx,
        icons: Icons::resolve(),
        socket_path: socket_path.clone(),
        status: None,
        autostart: autostart_path().exists(),
        relayout: false,
    };
    let handle = match tray.spawn() {
        Ok(handle) => handle,
        Err(e) => {
            // No StatusNotifier host (e.g. stock GNOME without the AppIndicator
            // extension) — the palette still works via the hotkey.
            eprintln!("[klip-gui] Tray icon unavailable: {e}");
            return;
        }
    };
    std::thread::spawn(move || sync_with_daemon(handle, socket_path));

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

/// Keep the tray's daemon state current: refresh on every daemon event, and
/// reconnect every few seconds while the daemon is down.
fn sync_with_daemon(handle: ksni::blocking::Handle<KlipTray>, socket_path: PathBuf) {
    let mut pictures = Pictures::default();
    loop {
        if !refresh(&handle, &socket_path, &mut pictures) {
            return; // tray is gone
        }
        if let Ok((reader, _shutdown)) = client::subscribe(&socket_path) {
            for line in reader.lines() {
                if line.is_err() || !refresh(&handle, &socket_path, &mut pictures) {
                    break;
                }
            }
            // Connection lost: show "not running" until the daemon is back
            if !refresh(&handle, &socket_path, &mut pictures) {
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Fetch daemon state into the tray. Returns `false` once the tray has shut down.
fn refresh(
    handle: &ksni::blocking::Handle<KlipTray>,
    socket_path: &PathBuf,
    pictures: &mut Pictures,
) -> bool {
    let status = client::status(socket_path)
        // A daemon older than this GUI (e.g. not yet restarted after an
        // upgrade) doesn't know `Status`; it can't pause either
        .or_else(|_| client::count(socket_path).map(|count| (count, false)))
        .ok()
        .map(|(count, paused)| {
            let all = client::list_entries(None, socket_path).unwrap_or_default();
            let entries = menu_entries(all);
            pictures.retain_for(&entries);
            let entries = entries.iter().map(|e| pictures.menu_clip(e)).collect();
            Status { entries, count, paused }
        });
    handle
        .update(move |tray| {
            if clip_rows(&tray.status) != clip_rows(&status) {
                tray.relayout = !tray.relayout;
            }
            tray.status = status;
        })
        .is_some()
}

/// What identifies the menu's clip rows: id, pinned, and thumbnail vs theme icon.
fn clip_rows(status: &Option<Status>) -> Option<Vec<(i64, bool, bool)>> {
    status.as_ref().map(|st| st.entries.iter().map(|c| (c.id, c.pinned, c.picture.is_some())).collect())
}

/// Up to [`MENU_PINNED`] pinned clips, then the most recent ones, [`MENU_CLIPS`]
/// in total. `all` comes from the daemon pinned-first, newest-first.
fn menu_entries(all: Vec<ClipEntry>) -> Vec<ClipEntry> {
    let (pinned, recent): (Vec<_>, Vec<_>) = all.into_iter().partition(|e| e.pinned);
    let pinned: Vec<_> = pinned.into_iter().take(MENU_PINNED).collect();
    let room = MENU_CLIPS - pinned.len();
    pinned.into_iter().chain(recent.into_iter().take(room)).collect()
}

/// One-line menu label for a clip.
/// One-line menu label for a text clip; `image_size` labels an image clip.
fn clip_label(entry: &ClipEntry, image_size: Option<(i32, i32)>) -> String {
    let text = if entry.is_image() {
        match image_size {
            Some((w, h)) => format!("Image · {w}×{h}"),
            None => "Image".into(),
        }
    } else {
        let line = entry.content.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        match line.char_indices().nth(LABEL_CHARS) {
            Some((i, _)) => format!("{}…", &line[..i]),
            None => line.to_string(),
        }
    };
    // A single underscore marks a mnemonic in DBusMenu labels
    text.replace('_', "__")
}

impl KlipTray {
    fn clip_icon(&self, clip: &MenuClip) -> &str {
        if clip.pinned {
            return &self.icons.pinned;
        }
        match clip.kind {
            Kind::Image => &self.icons.image,
            Kind::Link => &self.icons.link,
            Kind::Code => &self.icons.code,
            Kind::Email => &self.icons.email,
            Kind::Path => &self.icons.path,
            Kind::Text | Kind::Color => &self.icons.text,
        }
    }

    fn disabled_item(&self, label: &str) -> MenuItem<KlipTray> {
        StandardItem {
            label: label.into(),
            icon_name: self.icons.info.clone(),
            enabled: false,
            ..Default::default()
        }
        .into()
    }
}

// ── Pictures (thumbnails & swatches) ─────────────────────────────────────────

/// Pictures are drawn at this size; the panel scales them to its icon size
/// (32 px keeps them sharp on HiDPI).
const PICTURE_PX: i32 = 32;

/// Rendered pictures, keyed by clip content (an image's file name or a colour
/// code), so each is decoded once rather than on every refresh.
#[derive(Default)]
struct Pictures {
    cache: std::collections::HashMap<String, Picture>,
}

#[derive(Clone)]
struct Picture {
    png: Option<Vec<u8>>,
    /// Original width and height, for images.
    size: Option<(i32, i32)>,
}

impl Pictures {
    fn menu_clip(&mut self, entry: &ClipEntry) -> MenuClip {
        let kind = Kind::of(entry);
        let picture = match kind {
            Kind::Image | Kind::Color => Some(
                self.cache
                    .entry(entry.content.clone())
                    .or_insert_with(|| render_picture(entry, kind))
                    .clone(),
            ),
            _ => None,
        };
        MenuClip {
            id: entry.id,
            pinned: entry.pinned,
            kind,
            label: clip_label(entry, picture.as_ref().and_then(|p| p.size)),
            picture: picture.and_then(|p| p.png),
        }
    }

    /// Forget pictures of clips that are no longer listed.
    fn retain_for(&mut self, entries: &[ClipEntry]) {
        self.cache.retain(|key, _| entries.iter().any(|e| &e.content == key));
    }
}

fn render_picture(entry: &ClipEntry, kind: Kind) -> Picture {
    match kind {
        Kind::Image => match entry.image_path().and_then(|p| thumbnail_png(&p)) {
            Some((png, w, h)) => Picture { png: Some(png), size: Some((w, h)) },
            None => Picture { png: None, size: None },
        },
        _ => Picture { png: swatch_png(&entry.content), size: None },
    }
}

/// The image as a [`PICTURE_PX`] square tile: scaled until its shorter side
/// fills the square, then centre-cropped (a wide screenshot squeezed in whole
/// would be a thin, unreadable strip). Images smaller than the tile are
/// centred on a transparent square instead of being upscaled.
fn thumbnail_png(path: &std::path::Path) -> Option<(Vec<u8>, i32, i32)> {
    use gtk4::gdk_pixbuf::{Colorspace, Pixbuf};
    let (_, w, h) = Pixbuf::file_info(path)?;
    let canvas = Pixbuf::new(Colorspace::Rgb, true, 8, PICTURE_PX, PICTURE_PX)?;
    canvas.fill(0x0000_0000);
    let short = w.min(h);
    if short >= PICTURE_PX {
        let scale = PICTURE_PX as f64 / short as f64;
        let (sw, sh) = (((w as f64 * scale).round() as i32).max(PICTURE_PX), ((h as f64 * scale).round() as i32).max(PICTURE_PX));
        let scaled = Pixbuf::from_file_at_scale(path, sw, sh, false).ok()?.add_alpha(false, 0, 0, 0).ok()?;
        scaled.copy_area((sw - PICTURE_PX) / 2, (sh - PICTURE_PX) / 2, PICTURE_PX, PICTURE_PX, &canvas, 0, 0);
    } else {
        let small = Pixbuf::from_file(path).ok()?.add_alpha(false, 0, 0, 0).ok()?;
        let (sw, sh) = (small.width().min(PICTURE_PX), small.height().min(PICTURE_PX));
        small.copy_area(0, 0, sw, sh, &canvas, (PICTURE_PX - sw) / 2, (PICTURE_PX - sh) / 2);
    }
    let png = canvas.save_to_bufferv("png", &[]).ok()?;
    Some((png, w, h))
}

/// A square of the colour `#RGB`, `#RRGGBB` or `#RRGGBBAA`, with a thin grey
/// outline so dark colours stay visible on dark menus.
fn swatch_png(color: &str) -> Option<Vec<u8>> {
    use gtk4::gdk_pixbuf::{Colorspace, Pixbuf};
    let rgba = parse_hex_color(color)?;
    let canvas = Pixbuf::new(Colorspace::Rgb, true, 8, PICTURE_PX, PICTURE_PX)?;
    canvas.fill(0x0000_0000);
    let (margin, outline) = (4, 1);
    let side = PICTURE_PX - 2 * margin;
    canvas.new_subpixbuf(margin, margin, side, side).fill(0x8c8c_8cff);
    let inner = side - 2 * outline;
    canvas.new_subpixbuf(margin + outline, margin + outline, inner, inner).fill(rgba);
    canvas.save_to_bufferv("png", &[]).ok()
}

/// `#RGB`, `#RRGGBB` or `#RRGGBBAA` → `0xRRGGBBAA`.
fn parse_hex_color(color: &str) -> Option<u32> {
    let hex = color.trim().strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let full = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).collect::<String>() + "ff",
        6 => format!("{hex}ff"),
        8 => hex.to_string(),
        _ => return None,
    };
    u32::from_str_radix(&full, 16).ok()
}

fn clips_text(count: usize) -> String {
    if count == 1 { "1 clip".into() } else { format!("{count} clips") }
}


// ── Autostart ──────────────────────────────────────────────────────

/// `$XDG_CONFIG_HOME/autostart/klip.desktop`
fn autostart_path() -> PathBuf {
    let config_home = klip_common::config_path()
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_default();
    config_home.join("autostart").join("klip.desktop")
}

const AUTOSTART_DESKTOP: &str = "\
[Desktop Entry]
Type=Application
Name=Klip
Comment=Clipboard manager tray icon
Exec=klip --hidden
Icon=klip
X-GNOME-Autostart-enabled=true
";

fn set_autostart(enable: bool) -> std::io::Result<()> {
    let path = autostart_path();
    if enable {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, AUTOSTART_DESKTOP)
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, content: &str, pinned: bool) -> ClipEntry {
        ClipEntry {
            id,
            content: content.into(),
            mime_type: "text/plain".into(),
            pinned,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn menu_entries_caps_pinned_and_total() {
        let mut all: Vec<_> = (0..8).map(|i| entry(i, "p", true)).collect();
        all.extend((8..30).map(|i| entry(i, "r", false)));
        let menu = menu_entries(all);
        assert_eq!(menu.len(), MENU_CLIPS);
        assert_eq!(menu.iter().filter(|e| e.pinned).count(), MENU_PINNED);
        assert_eq!(menu[MENU_PINNED].id, 8);
    }

    #[test]
    fn clip_label_is_one_escaped_line() {
        assert_eq!(clip_label(&entry(1, "\n  snake_case \nmore", false), None), "snake__case");
        let long = "é".repeat(60);
        assert_eq!(clip_label(&entry(1, &long, false), None).chars().count(), LABEL_CHARS + 1);
        let mut image = entry(1, "abc.png", false);
        image.mime_type = "image/png".into();
        assert_eq!(clip_label(&image, Some((1920, 1080))), "Image · 1920×1080");
        assert_eq!(clip_label(&image, None), "Image");
    }

    #[test]
    fn hex_colors_parse() {
        assert_eq!(parse_hex_color("#ff8800"), Some(0xff88_00ff));
        assert_eq!(parse_hex_color(" #f80 "), Some(0xff88_00ff));
        assert_eq!(parse_hex_color("#11223344"), Some(0x1122_3344));
        assert_eq!(parse_hex_color("#ggg"), None);
        assert_eq!(parse_hex_color("ff8800"), None);
    }

    #[test]
    fn thumbnail_fits_a_square_and_keeps_size() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../icons/klip.png");
        let (png, w, h) = thumbnail_png(&path).unwrap();
        assert_eq!((w, h), (256, 256));
        let thumb = gtk4::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(png)).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (PICTURE_PX, PICTURE_PX));
    }

    #[test]
    fn swatch_is_a_png() {
        assert!(swatch_png("#ff8800").unwrap().starts_with(b"\x89PNG"));
    }
}
