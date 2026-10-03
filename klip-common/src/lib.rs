use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single clipboard entry stored in history.
///
/// For images (`mime_type` starting with `image/`), `content` is the file name
/// of the stored image inside [`images_dir`], not the image data itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipEntry {
    pub id: i64,
    pub content: String,
    pub mime_type: String,
    pub pinned: bool,
    pub created_at: String, // ISO-8601
    pub updated_at: String,
}

impl ClipEntry {
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }

    /// Path of the stored image file, for image entries.
    pub fn image_path(&self) -> Option<PathBuf> {
        self.is_image().then(|| images_dir().join(&self.content))
    }
}

/// Request sent from GUI to daemon over the Unix socket.
#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonRequest {
    /// Get all entries, optionally filtered by a search query.
    List { query: Option<String> },
    /// Pin/unpin an entry by ID.
    TogglePin { id: i64 },
    /// Delete an entry by ID.
    Delete { id: i64 },
    /// Clear unpinned history.
    ClearHistory,
    /// Copy an entry back to the system clipboard.
    Copy { id: i64 },
    /// Get the total count of stored entries.
    Count,
    /// Pause or resume recording new clips (not persisted across restarts).
    SetPaused { paused: bool },
    /// Get the entry count and whether capture is paused.
    Status,
    /// Turn this connection into an event stream: the daemon answers `Ok`, then
    /// writes one JSON-encoded [`DaemonEvent`] per line as history changes.
    Subscribe,
}

/// Response sent from daemon to GUI.
#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonResponse {
    Entries(Vec<ClipEntry>),
    Count(usize),
    Status { count: usize, paused: bool },
    Ok,
    Error(String),
}

/// Messages the daemon pushes to subscribed clients.
#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonEvent {
    EntryAdded(ClipEntry),
    EntryRemoved(i64),
    EntryUpdated(ClipEntry),
    /// All unpinned entries were removed.
    HistoryCleared,
    /// Capture was paused (`true`) or resumed (`false`).
    PausedChanged(bool),
}

// ── Paths ─────────────────────────────────────────────────────────────────────

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var(var).map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(fallback)
    })
}

/// `$XDG_DATA_HOME/klip` (default `~/.local/share/klip`): database and socket.
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").join("klip")
}

/// Where captured images are stored.
pub fn images_dir() -> PathBuf {
    data_dir().join("images")
}

pub fn socket_path() -> PathBuf {
    data_dir().join("klip.sock")
}

/// `$XDG_CONFIG_HOME/klip/config.toml` (default `~/.config/klip/config.toml`).
pub fn config_path() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("klip").join("config.toml")
}

// ── Config ────────────────────────────────────────────────────────────────────

/// User settings, read from [`config_path`]. Missing keys take their defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Maximum number of unpinned entries kept; older ones are deleted.
    /// `0` keeps everything.
    pub max_history: usize,
    /// Record copied images (screenshots, images copied from a browser, …).
    pub capture_images: bool,
    /// Images larger than this many megabytes are not recorded.
    pub max_image_mb: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_history: 1000,
            capture_images: true,
            max_image_mb: 20,
        }
    }
}

pub const DEFAULT_CONFIG_TOML: &str = "\
# Klip configuration. Restart klipd after editing:
#   systemctl --user restart klipd

# Maximum number of unpinned clips to keep (0 = unlimited). Pinned clips are
# never removed automatically.
max_history = 1000

# Record copied images (screenshots, images copied from a browser, ...).
capture_images = true

# Ignore images larger than this many megabytes.
max_image_mb = 20
";

impl Config {
    /// Load the config file, falling back to defaults if it is missing or
    /// invalid (an invalid file is reported in the returned warning).
    pub fn load() -> (Self, Option<String>) {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (Self::default(), Some(format!("invalid {}: {e}", path.display()))),
            },
            Err(_) => (Self::default(), None),
        }
    }

    /// Write the documented default config if none exists yet, so users can
    /// discover the settings.
    pub fn write_default_if_missing() -> std::io::Result<()> {
        let path = config_path();
        if path.exists() {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, DEFAULT_CONFIG_TOML)
    }

    pub fn max_image_bytes(&self) -> usize {
        (self.max_image_mb as usize).saturating_mul(1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_toml_matches_default_config() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG_TOML).unwrap();
        let def = Config::default();
        assert_eq!(parsed.max_history, def.max_history);
        assert_eq!(parsed.capture_images, def.capture_images);
        assert_eq!(parsed.max_image_mb, def.max_image_mb);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let parsed: Config = toml::from_str("max_history = 5").unwrap();
        assert_eq!(parsed.max_history, 5);
        assert!(parsed.capture_images);
    }
}
