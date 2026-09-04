//! Event-driven Wayland clipboard monitoring via `zwlr_data_control_v1`.
//!
//! Supported compositors:
//!   - KDE Plasma (Wayland) — KDE 5.20+ (2020)
//!   - GNOME Shell           — GNOME 43+ (Ubuntu 23.04+)
//!   - Sway, Hyprland, and all wlroots-based compositors
//!
//! Protocol flow (push model, zero polling):
//!   1. Bind `zwlr_data_control_manager_v1` and `wl_seat` from the registry
//!   2. Create a `zwlr_data_control_device_v1` for the seat
//!   3. Compositor sends events when clipboard changes:
//!        DataOffer { id }       ← new offer object being described
//!        Offer { mime_type }    ← repeated for each available MIME type
//!        Selection { id }       ← this offer IS the clipboard now
//!   4. On Selection, ask compositor to pipe `text/plain` to us, read it
//!
//! Returns `Err` if the compositor does not support the protocol, so the
//! caller can fall back to polling.

use anyhow::{anyhow, Result};
use klip_common::ClipEntry;
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::mpsc::Sender;
use wayland_client::{
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
};

// ── State ─────────────────────────────────────────────────────────────────────

struct AppState {
    tx: Sender<ClipEntry>,
    manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    device_created: bool,
    /// MIME types collected per offer while DataOffer events are being received.
    /// Key: offer proxy id (as raw pointer address used as unique id)
    pending_mimes: HashMap<usize, Vec<String>>,
    last_content: Option<String>,
}

impl AppState {
    fn new(tx: Sender<ClipEntry>) -> Self {
        Self {
            tx,
            manager: None,
            seat: None,
            device_created: false,
            pending_mimes: HashMap::new(),
            last_content: None,
        }
    }

    /// Create the data_control_device once both manager and seat are known.
    fn try_create_device(&mut self, qh: &QueueHandle<Self>) {
        if self.device_created {
            return;
        }
        if let (Some(mgr), Some(seat)) = (&self.manager, &self.seat) {
            mgr.get_data_device(seat, qh, ());
            self.device_created = true;
            log::debug!("[wayland_dc] data_control_device created");
        }
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
                "zwlr_data_control_manager_v1" => {
                    let mgr = registry.bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(
                        name,
                        version.min(2),
                        qh,
                        (),
                    );
                    state.manager = Some(mgr);
                    log::debug!("[wayland_dc] bound zwlr_data_control_manager_v1 v{}", version.min(2));
                    state.try_create_device(qh);
                }
                "wl_seat" => {
                    let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(8), qh, ());
                    state.seat = Some(seat);
                    log::debug!("[wayland_dc] bound wl_seat");
                    state.try_create_device(qh);
                }
                _ => {}
            }
        }
    }
}

// ── Manager dispatch — no events ──────────────────────────────────────────────

impl Dispatch<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // No events defined for this interface
    }
}

// ── Seat dispatch — no events we care about ───────────────────────────────────

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

// ── Data device dispatch — core clipboard events ──────────────────────────────

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _device: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // Step 1: compositor introduces a new offer object
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                let key = offer_key(&id);
                state.pending_mimes.insert(key, Vec::new());
            }

            // Step 2: compositor sets this offer as the current clipboard selection
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if let Some(offer) = id {
                    let key = offer_key(&offer);
                    if let Some(mimes) = state.pending_mimes.remove(&key) {
                        if let Some(content) = read_text_from_offer(&offer, &mimes) {
                            let content = content.trim_end_matches('\n').to_string();
                            if !content.is_empty()
                                && Some(&content) != state.last_content.as_ref()
                            {
                                log::debug!(
                                    "[wayland_dc] new clip ({} bytes)",
                                    content.len()
                                );
                                state.last_content = Some(content.clone());
                                let _ = state.tx.send(super::make_entry(content));
                            }
                        }
                    }
                    offer.destroy();
                }
            }

            // Compositor destroyed the device (e.g. seat removed)
            zwlr_data_control_device_v1::Event::Finished => {
                log::warn!("[wayland_dc] data_control_device finished — compositor revoked access");
            }

            // Primary selection (middle-click) — ignore
            zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {}

            _ => {}
        }
    }
}

// ── Offer dispatch — collect MIME types ───────────────────────────────────────

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for AppState {
    fn event(
        state: &mut Self,
        offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            let key = offer_key(offer);
            state
                .pending_mimes
                .entry(key)
                .or_default()
                .push(mime_type);
        }
    }
}

// ── Helper: stable key for an offer proxy ────────────────────────────────────

fn offer_key(offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1) -> usize {
    offer.id().protocol_id() as usize
}

// ── Helper: read text content from an offer via a pipe ───────────────────────

/// Try each preferred MIME type in order, returning the first that yields text.
fn read_text_from_offer(
    offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    mimes: &[String],
) -> Option<String> {
    // Priority order — most specific first
    const PREFERRED: &[&str] = &[
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "STRING",
        "TEXT",
    ];

    let mime = PREFERRED
        .iter()
        .find(|&&preferred| mimes.iter().any(|m| m.eq_ignore_ascii_case(preferred)))?;

    // Create a pipe: compositor writes to write_fd, we read from read_fd
    let (read_fd, write_fd) = make_pipe().ok()?;

    // Ask compositor to write clipboard content into write_fd
    offer.receive(mime.to_string(), write_fd.as_fd());

    // Close our copy of write_fd — we must do this BEFORE reading so we get
    // EOF when the compositor finishes writing (not hang forever).
    drop(write_fd);

    // Read all content from the pipe.
    // The compositor will write and close its end after we flush (which
    // blocking_dispatch does automatically on the next iteration, but since
    // we dropped write_fd the kernel will EOF us correctly).
    let mut file = unsafe { std::fs::File::from_raw_fd(read_fd.into_raw_fd()) };

    let mut content = String::new();
    match file.read_to_string(&mut content) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(content),
    }
}

// ── Helper: create an O_CLOEXEC pipe ─────────────────────────────────────────

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

// ── Public API ────────────────────────────────────────────────────────────────

/// Try to start event-driven Wayland clipboard monitoring.
///
/// Connects to the Wayland display, checks that the compositor supports
/// `zwlr_data_control_v1`, then spawns a background thread that blocks
/// on compositor events (zero CPU when idle).
///
/// Returns `Ok(())` on success. Returns `Err` if:
/// - `$WAYLAND_DISPLAY` is not set
/// - The compositor does not support `zwlr_data_control_v1`
/// - The initial Wayland round-trip fails
pub fn try_watch(tx: Sender<ClipEntry>) -> Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow!("Cannot connect to Wayland display: {e}"))?;

    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    display.get_registry(&qh, ());

    let mut state = AppState::new(tx);

    // Round-trip 1: receive all Global events → binds manager + seat
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| anyhow!("Wayland round-trip failed: {e}"))?;

    // Verify the compositor actually supports the protocol
    if state.manager.is_none() {
        return Err(anyhow!(
            "Compositor does not support zwlr_data_control_v1 \
             (GNOME < 43 or unsupported compositor)"
        ));
    }

    // Round-trip 2: send get_data_device request + wait for acknowledgment
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| anyhow!("Wayland round-trip 2 failed: {e}"))?;

    log::info!("[wayland_dc] zwlr_data_control_v1 active — event-driven clipboard monitoring");

    // Spawn the event loop thread — blocks in kernel until compositor sends events
    std::thread::spawn(move || {
        loop {
            match event_queue.blocking_dispatch(&mut state) {
                Ok(_) => {}
                Err(e) => {
                    log::error!("[wayland_dc] Wayland dispatch error: {e}");
                    break;
                }
            }
        }
        log::warn!("[wayland_dc] Event loop exited");
    });

    Ok(())
}
