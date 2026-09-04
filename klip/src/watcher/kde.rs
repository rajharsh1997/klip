use anyhow::Result;
use klip_common::ClipEntry;
use std::io::BufRead;
use std::sync::mpsc::Sender;
use std::thread;

/// Try to start KDE D-Bus clipboard monitoring.
/// Returns Ok(()) if Klipper is running and dbus-monitor was spawned.
/// This is event-driven — zero CPU, instant notification.
pub fn try_watch(tx: Sender<ClipEntry>) -> Result<()> {
    log::info!("Trying KDE Klipper D-Bus clipboard monitoring...");

    // Check if Klipper is running
    let check = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.DBus",
            "--print-reply",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.NameHasOwner",
            "string:org.kde.klipper",
        ])
        .output();
        
    match check {
        Ok(output) if output.status.success() => {
            let reply = String::from_utf8_lossy(&output.stdout);
            if !reply.contains("boolean true") {
                log::debug!("KDE Klipper is not running (NameHasOwner returned false)");
                return Err(anyhow::anyhow!("Klipper not available"));
            }
        }
        _ => {
            log::debug!("dbus-send failed or is unavailable");
            return Err(anyhow::anyhow!("Klipper not available"));
        }
    }

    // Spawn dbus-monitor to listen for clipboardHistoryUpdated signals
    let mut child = std::process::Command::new("dbus-monitor")
        .args([
            "--session",
            "interface='org.kde.klipper.klipper',member='clipboardHistoryUpdated'",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("dbus-monitor failed: {}", e))?;

    thread::spawn(move || {
        log::info!("KDE D-Bus clipboard monitoring active");
        let mut last_content: Option<String> = None;
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => return,
        };
        let reader = std::io::BufReader::new(stdout);

        // Read dbus-monitor output line by line
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            // clipboardHistoryUpdated signal received — clipboard changed
            if line.contains("clipboardHistoryUpdated") || line.contains("member=") {
                if let Some(content) = super::read_clipboard_wl_paste() {
                    if Some(&content) != last_content.as_ref() {
                        last_content = Some(content.clone());
                        let _ = tx.send(super::make_entry(content));
                    }
                }
            }
        }
    });

    Ok(())
}
