use serde::{Deserialize, Serialize};

/// A single clipboard entry stored in history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipEntry {
    pub id: i64,
    pub content: String,
    pub mime_type: String,
    pub pinned: bool,
    pub created_at: String, // ISO-8601
    pub updated_at: String,
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
}

/// Response sent from daemon to GUI.
#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonResponse {
    Entries(Vec<ClipEntry>),
    Count(usize),
    Ok,
    Error(String),
}

/// Messages the daemon can push to connected clients proactively.
#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonEvent {
    EntryAdded(ClipEntry),
    EntryRemoved(i64),
    EntryUpdated(ClipEntry),
}