use std::path::PathBuf;

use rusqlite::Connection;

use crate::error::Result;

pub const DEFAULT_DB_PATH: &str = "~/.conversation-search/index.db";

/// Expand ~ to home directory.
pub fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Create a SQLite connection with standard PRAGMA settings.
pub fn connect(db_path: &str, readonly: bool) -> Result<Connection> {
    let resolved = expand_path(db_path);

    let conn = if readonly {
        let uri = format!("file:{}?mode=ro", resolved.display());
        Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?
    } else {
        // Ensure parent directory exists
        if let Some(parent) = resolved.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Connection::open(&resolved)?
    };

    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=30000;
         PRAGMA foreign_keys=OFF;",
    )?;

    Ok(conn)
}

/// Get the default database path (expanded).
pub fn default_db_path() -> PathBuf {
    expand_path(DEFAULT_DB_PATH)
}
