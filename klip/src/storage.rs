use anyhow::Result;
use klip_common::ClipEntry;
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Storage {
    conn: Mutex<Connection>,
    images_dir: PathBuf,
}

/// Result of [`Storage::insert`].
pub struct Inserted {
    pub entry: ClipEntry,
    /// `false` if the clip was already in history and only moved to the top.
    pub is_new: bool,
}

impl Storage {
    pub fn new(data_dir: PathBuf, images_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&images_dir)?;
        let db_path = data_dir.join("klip.db");

        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS clips (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                content     TEXT    NOT NULL,
                mime_type   TEXT    NOT NULL DEFAULT 'text/plain',
                pinned      INTEGER NOT NULL DEFAULT 0,
                created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_clips_pinned ON clips(pinned);
            CREATE INDEX IF NOT EXISTS idx_clips_created ON clips(created_at DESC);",
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
            images_dir,
        })
    }

    /// Store an image: the bytes go to a content-addressed file in the images
    /// dir, and the row's `content` is that file name, so identical images
    /// dedup like identical text.
    pub fn insert_image(&self, mime_type: &str, data: &[u8]) -> Result<Inserted> {
        let ext = mime_type.strip_prefix("image/").unwrap_or("bin");
        let name = format!("{:016x}-{}.{ext}", fnv1a64(data), data.len());
        let path = self.images_dir.join(&name);
        if !path.exists() {
            // Write then rename, so a crash never leaves a truncated image
            let tmp = self.images_dir.join(format!(".{name}.tmp"));
            std::fs::write(&tmp, data)?;
            std::fs::rename(&tmp, &path)?;
        }
        self.insert(&name, mime_type)
    }

    pub fn insert(&self, content: &str, mime_type: &str) -> Result<Inserted> {
        let conn = self.conn.lock().unwrap();

        // Avoid duplicates: if same content exists, update its timestamp
        let existing: Option<(i64, bool)> = conn
            .query_row(
                "SELECT id, pinned FROM clips WHERE content = ?1 AND mime_type = ?2 LIMIT 1",
                params![content, mime_type],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((id, _pinned)) = existing {
            conn.execute(
                "UPDATE clips SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
                params![id],
            )?;
            // Drop the lock before calling get_by_id to avoid deadlock (Mutex is not reentrant)
            drop(conn);
            return Ok(Inserted { entry: self.get_by_id(id)?, is_new: false });
        }

        conn.execute(
            "INSERT INTO clips (content, mime_type) VALUES (?1, ?2)",
            params![content, mime_type],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);
        Ok(Inserted { entry: self.get_by_id(id)?, is_new: true })
    }

    pub fn list(&self, query: Option<&str>) -> Result<Vec<ClipEntry>> {
        let conn = self.conn.lock().unwrap();

        if let Some(q) = query {
            if !q.is_empty() {
                let pattern = format!("%{}%", q.replace('%', "\\%").replace('_', "\\_"));
                let mut stmt = conn.prepare(
                    "SELECT id, content, mime_type, pinned, created_at, updated_at
                     FROM clips
                     WHERE content LIKE ?1 ESCAPE '\\'
                     ORDER BY pinned DESC, updated_at DESC",
                )?;
                let rows = stmt.query_map(params![pattern], Self::row_to_entry)?;
                return rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into);
            }
        }

        let mut stmt = conn.prepare(
            "SELECT id, content, mime_type, pinned, created_at, updated_at
             FROM clips
             ORDER BY pinned DESC, updated_at DESC
             LIMIT 500",
        )?;
        let rows = stmt.query_map([], Self::row_to_entry)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn toggle_pin(&self, id: i64) -> Result<ClipEntry> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE clips SET pinned = CASE WHEN pinned = 0 THEN 1 ELSE 0 END, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        drop(conn);
        self.get_by_id(id)
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        self.delete_where("id = ?1", params![id])?;
        Ok(())
    }

    pub fn clear_history(&self) -> Result<usize> {
        Ok(self.delete_where("pinned = 0", [])?.len())
    }

    /// Delete the oldest unpinned entries beyond `max_history` (0 = no limit).
    /// Returns the ids of the deleted entries.
    pub fn prune(&self, max_history: usize) -> Result<Vec<i64>> {
        if max_history == 0 {
            return Ok(Vec::new());
        }
        self.delete_where(
            "pinned = 0 AND id NOT IN
                (SELECT id FROM clips WHERE pinned = 0 ORDER BY updated_at DESC LIMIT ?1)",
            params![max_history as i64],
        )
    }

    /// Delete matching rows and the image files no remaining row refers to.
    fn delete_where(&self, cond: &str, args: impl rusqlite::Params + Clone) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, content, mime_type FROM clips WHERE {cond}"
        ))?;
        let doomed = stmt
            .query_map(args.clone(), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        if doomed.is_empty() {
            return Ok(Vec::new());
        }
        conn.execute(&format!("DELETE FROM clips WHERE {cond}"), args)?;

        for (_, content, mime) in doomed.iter().filter(|(_, _, m)| m.starts_with("image/")) {
            let still_used: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM clips WHERE content = ?1 AND mime_type = ?2)",
                params![content, mime],
                |r| r.get(0),
            )?;
            if !still_used {
                let _ = std::fs::remove_file(self.images_dir.join(content));
            }
        }
        Ok(doomed.into_iter().map(|(id, _, _)| id).collect())
    }

    /// Read an image entry's stored bytes.
    pub fn image_data(&self, entry: &ClipEntry) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.images_dir.join(&entry.content))?)
    }

    pub fn count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    pub fn get_by_id(&self, id: i64) -> Result<ClipEntry> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, content, mime_type, pinned, created_at, updated_at FROM clips WHERE id = ?1",
            params![id],
            Self::row_to_entry,
        )
        .map_err(Into::into)
    }

    fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<ClipEntry> {
        Ok(ClipEntry {
            id: row.get(0)?,
            content: row.get(1)?,
            mime_type: row.get(2)?,
            pinned: row.get::<_, i64>(3)? != 0,
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
        })
    }
}

/// FNV-1a, 64-bit — a stable content hash for naming image files.
fn fnv1a64(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage(name: &str) -> (Storage, PathBuf) {
        let dir = std::env::temp_dir().join(format!("klip-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let images = dir.join("images");
        (Storage::new(dir.clone(), images.clone()).unwrap(), images)
    }

    #[test]
    fn insert_dedups_and_reports_new() {
        let (s, _) = temp_storage("dedup");
        assert!(s.insert("a", "text/plain").unwrap().is_new);
        assert!(!s.insert("a", "text/plain").unwrap().is_new);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn prune_keeps_newest_unpinned_and_all_pinned() {
        let (s, _) = temp_storage("prune");
        let first = s.insert("0", "text/plain").unwrap().entry;
        s.toggle_pin(first.id).unwrap();
        for i in 1..=5 {
            s.insert(&i.to_string(), "text/plain").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let removed = s.prune(2).unwrap();
        assert_eq!(removed.len(), 3);
        let left: Vec<String> = s.list(None).unwrap().into_iter().map(|e| e.content).collect();
        assert_eq!(left, ["0", "5", "4"]);
        assert!(s.prune(0).unwrap().is_empty());
    }

    #[test]
    fn image_files_follow_their_rows() {
        let (s, images) = temp_storage("images");
        let png = b"\x89PNG fake image bytes";
        let e = s.insert_image("image/png", png).unwrap().entry;
        assert!(e.content.ends_with(".png"));
        assert_eq!(s.image_data(&e).unwrap(), png);
        assert!(!s.insert_image("image/png", png).unwrap().is_new);
        s.delete(e.id).unwrap();
        assert!(!images.join(&e.content).exists());
    }
}
