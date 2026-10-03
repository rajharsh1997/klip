//! Right-click (or Menu key / Shift+F10) menu for palette rows. Every item
//! shows its keyboard shortcut, so the menu doubles as shortcut help.
//!
//! Built from plain buttons in a `Popover` rather than a `PopoverMenu`: the
//! latter's internal scrolled window was allocated less than its natural
//! height on KDE, cutting off the last item.

use crate::Palette;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use klip_common::ClipEntry;
use std::rc::Rc;
use std::time::Duration;

/// What a menu item does to the entry the menu was opened for.
#[derive(Clone, Copy)]
enum Action {
    Copy,
    OpenLink,
    TogglePin,
    Delete,
    ClearHistory,
}

impl Palette {
    /// Create the popover and the right-click gesture. Called once per window.
    pub(crate) fn install_row_menu(self: &Rc<Self>) {
        // One popover per window, refilled on each open. It is a manually
        // parented child, so it must be unparented before the list box goes.
        let popover = &self.row_menu;
        popover.add_css_class("klip-menu");
        popover.set_has_arrow(false);
        popover.set_position(gtk4::PositionType::Bottom);
        // Open rightwards from the click point instead of centred on it, so the
        // menu stays over the (narrow) palette
        popover.set_halign(gtk4::Align::Start);
        popover.set_parent(&self.list_box);
        {
            let popover = popover.clone();
            self.list_box.connect_destroy(move |_| popover.unparent());
        }
        let weak = Rc::downgrade(self);
        popover.connect_closed(move |_| {
            let Some(p) = weak.upgrade() else { return };
            p.menu_open.set(false);
            // If the menu closed because the user clicked outside the palette,
            // the palette has lost focus: dismiss it as usual
            let window = p.window.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                if let Some(w) = window.upgrade() {
                    if !w.is_active() && w.is_visible() {
                        w.close();
                    }
                }
            });
        });

        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        let weak = Rc::downgrade(self);
        gesture.connect_pressed(move |gesture, _, x, y| {
            let Some(p) = weak.upgrade() else { return };
            let Some(row) = p.list_box.row_at_y(y as i32) else { return };
            let idx = p.shown.borrow().iter().position(|(_, r)| *r == row);
            if let Some(idx) = idx {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                p.show_row_menu(idx, Some((x, y)));
            }
        });
        self.list_box.add_controller(gesture);
    }

    /// Open the menu for the entry at `idx`: at `pos` (list-box coordinates)
    /// for a click, or below the row when opened from the keyboard.
    pub(crate) fn show_row_menu(self: &Rc<Self>, idx: usize, pos: Option<(f64, f64)>) {
        let Some((entry, row)) = self.shown.borrow().get(idx).cloned() else { return };
        self.select(idx);
        self.row_menu.set_child(Some(&self.build_menu(&entry, idx)));

        let rect = match pos {
            Some((x, y)) => gdk::Rectangle::new(x as i32, y as i32, 1, 1),
            None => row
                .compute_bounds(&self.list_box)
                .map(|b| gdk::Rectangle::new(b.x() as i32 + 32, (b.y() + b.height()) as i32, 1, 1))
                .unwrap_or_else(|| gdk::Rectangle::new(32, 0, 1, 1)),
        };
        self.row_menu.set_pointing_to(Some(&rect));
        self.menu_open.set(true);
        self.row_menu.popup();
    }

    fn build_menu(self: &Rc<Self>, entry: &ClipEntry, idx: usize) -> gtk4::Box {
        let quick = (idx < 9).then(|| format!("Alt+{}", idx + 1));
        let mut sections: Vec<Vec<(&str, Option<&str>, Action)>> = vec![vec![("Copy", Some("Enter"), Action::Copy)]];
        if let Some(quick) = &quick {
            sections[0].push(("Quick Copy", Some(quick.as_str()), Action::Copy));
        }
        if entry.mime_type == "text/uri-list" {
            sections[0].push(("Open Link", None, Action::OpenLink));
        }
        let pin = if entry.pinned { "Unpin" } else { "Pin" };
        sections.push(vec![(pin, Some("Alt+P"), Action::TogglePin), ("Delete", Some("Alt+Delete"), Action::Delete)]);
        sections.push(vec![("Clear Unpinned History", Some("Ctrl+Backspace"), Action::ClearHistory)]);

        let menu = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        for (i, section) in sections.into_iter().enumerate() {
            if i > 0 {
                menu.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
            }
            for (label, accel, action) in section {
                menu.append(&self.menu_button(label, accel, action, entry.id));
            }
        }
        menu
    }

    fn menu_button(self: &Rc<Self>, label: &str, accel: Option<&str>, action: Action, id: i64) -> gtk4::Button {
        let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 24);
        let text = gtk4::Label::new(Some(label));
        text.set_halign(gtk4::Align::Start);
        text.set_hexpand(true);
        content.append(&text);
        if let Some(accel) = accel {
            let hint = gtk4::Label::new(Some(accel));
            hint.add_css_class("accel");
            content.append(&hint);
        }

        let button = gtk4::Button::new();
        button.set_child(Some(&content));
        button.add_css_class("flat");
        button.add_css_class("menu-item");
        // The popover focuses its first item on open, and focus is highlighted
        // like hover: move focus with the pointer so only one item is lit
        let motion = gtk4::EventControllerMotion::new();
        {
            let button = button.downgrade();
            motion.connect_enter(move |_, _, _| {
                if let Some(b) = button.upgrade() {
                    b.grab_focus();
                }
            });
        }
        button.add_controller(motion);
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            let Some(p) = weak.upgrade() else { return };
            p.row_menu.popdown();
            match action {
                Action::Copy => p.copy(id),
                Action::OpenLink => p.open_link(id),
                Action::TogglePin => p.toggle_pin(id),
                Action::Delete => p.delete(id),
                Action::ClearHistory => p.clear_history(),
            }
        });
        button
    }
}
