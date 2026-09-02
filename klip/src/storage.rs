use anyhow::Result;
use klip_common::ClipEntry;
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
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
        })
    }

    pub fn insert(&self, content: &str, mime_type: &str) -> Result<ClipEntry> {
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
            return self.get_by_id(id);
        }

        conn.execute(
            "INSERT INTO clips (content, mime_type) VALUES (?1, ?2)",
            params![content, mime_type],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.get_by_id(id)
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
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn clear_history(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count = conn.execute("DELETE FROM clips WHERE pinned = 0", [])?;
        Ok(count)
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