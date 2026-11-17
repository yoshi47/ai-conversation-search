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
        // Why not a `file:...?mode=ro` URI: `#`, `?` and `%` in the path would be parsed
        // as URI syntax and open a different file. The flag alone makes it read-only.
        Connection::open_with_flags(
            &resolved,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
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

    // Scalar function used by the bigram drain/backfill. Registering on every
    // connection (read-only included) costs nothing and lets any writer drain.
    crate::bigram::register_sql_function(&conn)?;

    Ok(conn)
}

/// Get the default database path (expanded).
pub fn default_db_path() -> PathBuf {
    expand_path(DEFAULT_DB_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#`, `?` and `%` are URI syntax; a read-only open must still reach this exact file.
    #[test]
    fn test_readonly_connect_opens_path_with_uri_metacharacters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a#b?c%41.db");
        let path_str = path.to_str().unwrap();
        connect(path_str, false)
            .unwrap()
            .execute_batch("CREATE TABLE marker (x); INSERT INTO marker VALUES (1);")
            .unwrap();

        let conn = connect(path_str, true).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM marker", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(conn.execute("INSERT INTO marker VALUES (2)", []).is_err());
    }
}
