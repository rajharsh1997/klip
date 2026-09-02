# Klip GUI Window Visibility Issue

## Environment
- **Display Server**: Wayland (wayland-0)
- **Session Type**: wayland
- **OS**: Fedora 43
- **GTK4 Version**: 0.9.7 (gtk4-rs)
- **Desktop**: GNOME (likely)

## Symptoms
- `klip-gui` process starts successfully (visible in `ps aux`)
- Taskbar icon appears for the app
- **No window is visible on screen** — the floating palette never appears
- No crash, no error output on stderr
- The daemon (`klipd`) runs fine and the socket connection works

## What We've Tried

### 1. `gtk4::init()` + `Window::new()` + `MainLoop`
```rust
fn main() -> glib::ExitCode {
    gtk4::init().expect("Failed to initialize GTK");
    build_ui();
    glib::MainLoop::new(None, false).run();
    glib::ExitCode::SUCCESS
}
```
- Used `window.present()` to show
- Also tried `window.show()` + `window.present()`
- Window has decorations (`set_decorated(true)`)
- **Result**: Taskbar icon appears, no visible window

### 2. `Application` + `ApplicationWindow` (single-instance)
```rust
fn main() -> glib::ExitCode {
    let app = gtk4::Application::new(
        Some("com.klip.clipboard-manager"),
        gio::ApplicationFlags::empty(),
    );
    app.connect_activate(|app| { build_ui(app); });
    app.run()
}
```
- Used `ApplicationWindow::new(app)`
- **Result**: Same — taskbar icon, no visible window

### 3. `Application` with `NON_UNIQUE` flag
```rust
gio::ApplicationFlags::NON_UNIQUE
```
- **Result**: Same issue

### 4. Removed auto-dismiss-on-focus-loss
- Removed `EventControllerFocus` entirely to rule out immediate close
- **Result**: Still no window

### 5. Window type hints
- Tried `window.set_type_hint(gdk::SurfaceTypeHint::Dialog)` — but `SurfaceTypeHint` doesn't exist in gdk4 0.9.7

## Current Code (simplified)

```rust
fn main() -> glib::ExitCode {
    gtk4::init().expect("Failed to initialize GTK");
    build_ui();
    glib::MainLoop::new(None, false).run();
    glib::ExitCode::SUCCESS
}

fn build_ui() {
    let window = gtk4::Window::new();
    window.set_title(Some("Klip"));
    window.set_default_size(420, 500);
    window.set_resizable(true);
    window.set_decorated(true);
    window.set_icon_name(Some("edit-paste-symbolic"));

    // ... build UI, add widgets ...

    window.connect_map(|_win| {
        // grab focus
    });

    window.present();
}
```

## Suspected Root Causes

1. **Wayland compositor not mapping the window**: GNOME on Wayland may refuse to show a window that doesn't have a proper `Application` + `app_id` association, or that lacks certain surface hints.

2. **Window appearing off-screen**: The window might be positioned at (0,0) or outside the visible area. On Wayland, the compositor controls window positioning.

3. **Missing `app_id`**: GTK4 Wayland needs `window.set_startup_id()` or the application ID from `Application` to properly associate the window.

4. **`gtk4::init()` vs `Application` conflict**: Calling `gtk4::init()` manually before `Application::run()` may cause issues on Wayland.

## Questions / Things to Try

- Does the window appear if we use `GtkWindow` with `gtk_window_set_keep_above()`? (Removed in GTK4, but maybe via `Toplevel` surface?)
- Does it work under XWayland? (Try `GDK_BACKEND=x11 ./klip-gui`)
- Does `window.set_startup_id()` help on Wayland?
- Does the window appear if we set `window.set_visible(true)` explicitly?
- Is there a GNOME extension blocking popup windows?