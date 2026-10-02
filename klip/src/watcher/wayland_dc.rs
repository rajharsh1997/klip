//! Event-driven Wayland clipboard monitoring via the data-control protocols.
//!
//! Two structurally identical protocols are supported, preferred in this order:
//!   - `ext_data_control_v1`  — the standardised version: KDE Plasma 6.2+,
//!     Sway 1.10+, Hyprland, niri, COSMIC, …
//!   - `zwlr_data_control_v1` — the older wlroots version: KDE 5.20+, Sway,
//!     Hyprland and other wlroots-based compositors
//!
//! GNOME (Mutter) implements neither, so `try_watch` returns `Err` there and
//! the caller falls back to another backend.
//!
//! Protocol flow (push model, zero polling):
//!   1. Bind the data-control manager and `wl_seat` from the registry
//!   2. Create a data-control device for the seat
//!   3. Compositor sends events when the clipboard changes:
//!        DataOffer { id }       ← new offer object being described
//!        Offer { mime_type }    ← repeated for each available MIME type
//!        Selection { id }       ← this offer IS the clipboard now
//!   4. On Selection, ask the source client to write text into a pipe, read it

use anyhow::{anyhow, Result};
use klip_common::ClipEntry;
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use wayland_client::{
    event_created_child,
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
};

/// Give up on a source client that doesn't finish writing within this time.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Ignore text clips larger than this.
const MAX_CLIP_BYTES: usize = 16 * 1024 * 1024;

/// Text MIME types in priority order — most specific first.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

// ── State ─────────────────────────────────────────────────────────────────────

struct AppState {
    tx: Sender<ClipEntry>,
    ext_manager: Option<ext_data_control_manager_v1::ExtDataControlManagerV1>,
    wlr_manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    /// MIME types advertised per offer, keyed by the offer's protocol id.
    pending_mimes: HashMap<u32, Vec<String>>,
    last_content: Option<String>,
}

impl AppState {
    fn new(tx: Sender<ClipEntry>) -> Self {
        Self {
            tx,
            ext_manager: None,
            wlr_manager: None,
            seat: None,
            pending_mimes: HashMap::new(),
            last_content: None,
        }
    }

    /// The clipboard selection changed to the offer with `key`. `receive` asks
    /// the source client to write the given MIME type into a file descriptor.
    fn on_selection(
        &mut self,
        conn: &Connection,
        key: u32,
        receive: impl FnOnce(String, BorrowedFd<'_>),
    ) {
        let Some(mimes) = self.pending_mimes.remove(&key) else {
            return;
        };
        let Some(mime) = TEXT_MIMES
            .iter()
            .find(|&&want| mimes.iter().any(|m| m.eq_ignore_ascii_case(want)))
        else {
            log::debug!("[wayland_dc] selection has no text type ({mimes:?})");
            return;
        };

        let Ok((read_fd, write_fd)) = make_pipe() else {
            return;
        };
        receive(mime.to_string(), write_fd.as_fd());
        // The request is only queued so far — flush it to the compositor, and
        // close our copy of the write end so we see EOF when the source is done.
        if let Err(e) = conn.flush() {
            log::warn!("[wayland_dc] flush failed: {e}");
            return;
        }
        drop(write_fd);

        let Some(bytes) = read_with_timeout(read_fd) else {
            log::debug!("[wayland_dc] reading selection failed or timed out");
            return;
        };
        let content = String::from_utf8_lossy(&bytes).into_owned();
        if content.trim().is_empty() || Some(&content) == self.last_content.as_ref() {
            return;
        }
        log::debug!("[wayland_dc] new clip ({} bytes)", content.len());
        self.last_content = Some(content.clone());
        let _ = self.tx.send(super::make_entry(content));
    }
}

// ── Registry dispatch — bind globals ──────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "ext_data_control_manager_v1" => {
                    state.ext_manager = Some(registry.bind(name, version.min(1), qh, ()));
                    log::debug!("[wayland_dc] bound ext_data_control_manager_v1");
                }
                "zwlr_data_control_manager_v1" => {
                    state.wlr_manager = Some(registry.bind(name, version.min(2), qh, ()));
                    log::debug!("[wayland_dc] bound zwlr_data_control_manager_v1");
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(8), qh, ()));
                    log::debug!("[wayland_dc] bound wl_seat");
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ── Data-control dispatch — identical for the ext and wlr protocol families ──

macro_rules! impl_data_control {
    ($manager:ident :: $Manager:ident, $device:ident :: $Device:ident, $offer:ident :: $Offer:ident) => {
        impl Dispatch<$manager::$Manager, ()> for AppState {
            fn event(
                _: &mut Self,
                _: &$manager::$Manager,
                _: $manager::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                // No events defined for this interface
            }
        }

        impl Dispatch<$device::$Device, ()> for AppState {
            fn event(
                state: &mut Self,
                _: &$device::$Device,
                event: $device::Event,
                _: &(),
                conn: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    // A new offer is being introduced; its MIME types follow
                    $device::Event::DataOffer { id } => {
                        state.pending_mimes.insert(id.id().protocol_id(), Vec::new());
                    }
                    // The offer became the clipboard selection
                    $device::Event::Selection { id: Some(offer) } => {
                        let key = offer.id().protocol_id();
                        state.on_selection(conn, key, |mime, fd| offer.receive(mime, fd));
                        offer.destroy();
                    }
                    // Primary selection (middle-click) — ignore, but free the offer
                    $device::Event::PrimarySelection { id: Some(offer) } => {
                        state.pending_mimes.remove(&offer.id().protocol_id());
                        offer.destroy();
                    }
                    $device::Event::Finished => {
                        log::warn!("[wayland_dc] data-control device finished — compositor revoked access");
                    }
                    _ => {}
                }
            }

            event_created_child!(AppState, $device::$Device, [
                $device::EVT_DATA_OFFER_OPCODE => ($offer::$Offer, ()),
            ]);
        }

        impl Dispatch<$offer::$Offer, ()> for AppState {
            fn event(
                state: &mut Self,
                offer: &$offer::$Offer,
                event: $offer::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let $offer::Event::Offer { mime_type } = event {
                    state
                        .pending_mimes
                        .entry(offer.id().protocol_id())
                        .or_default()
                        .push(mime_type);
                }
            }
        }
    };
}

impl_data_control!(
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1
);
impl_data_control!(
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1
);

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if ret != 0 {
        return Err(anyhow!("pipe2 failed: {}", std::io::Error::last_os_error()));
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read_fd, write_fd))
}

/// Read a pipe to EOF, giving up after `READ_TIMEOUT` or `MAX_CLIP_BYTES` so a
/// misbehaving source client can't stall the watcher.
fn read_with_timeout(fd: OwnedFd) -> Option<Vec<u8>> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut file = std::fs::File::from(fd);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let mut pfd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ret = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
        if ret < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if ret == 0 {
            return None; // timed out
        }
        match file.read(&mut chunk) {
            Ok(0) => return Some(buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_CLIP_BYTES {
                    return None;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Try to start event-driven Wayland clipboard monitoring.
///
/// Returns `Err` if `$WAYLAND_DISPLAY` is unusable or the compositor supports
/// neither data-control protocol. On success a background thread blocks on
/// compositor events (zero CPU when idle).
pub fn try_watch(tx: Sender<ClipEntry>) -> Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow!("Cannot connect to Wayland display: {e}"))?;

    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();
    conn.display().get_registry(&qh, ());

    let mut state = AppState::new(tx);

    // Round-trip 1: receive all Global events → bind managers + seat
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| anyhow!("Wayland round-trip failed: {e}"))?;

    let seat = state.seat.clone().ok_or_else(|| anyhow!("Compositor has no wl_seat"))?;
    let protocol = if let Some(mgr) = &state.ext_manager {
        mgr.get_data_device(&seat, &qh, ());
        "ext_data_control_v1"
    } else if let Some(mgr) = &state.wlr_manager {
        mgr.get_data_device(&seat, &qh, ());
        "zwlr_data_control_v1"
    } else {
        return Err(anyhow!(
            "Compositor supports neither ext_data_control_v1 nor zwlr_data_control_v1"
        ));
    };

    // Round-trip 2: create the device; the current selection arrives right away
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| anyhow!("Wayland round-trip 2 failed: {e}"))?;

    log::info!("[wayland_dc] {protocol} active — event-driven clipboard monitoring");

    // Event loop thread — blocks in the kernel until the compositor sends events
    std::thread::spawn(move || {
        loop {
            if let Err(e) = event_queue.blocking_dispatch(&mut state) {
                log::error!("[wayland_dc] Wayland dispatch error: {e}");
                break;
            }
        }
        log::warn!("[wayland_dc] Event loop exited");
    });

    Ok(())
}
