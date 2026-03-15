use rusqlite::Connection;

use crate::error::Result;

const SCHEMA_SQL: &str = include_str!("../data/schema.sql");

/// Initialize the database schema and run migrations.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;

    // Migration: Add is_meta_conversation if missing
    match conn.execute(
        "ALTER TABLE messages ADD COLUMN is_meta_conversation BOOLEAN DEFAULT FALSE",
        [],
    ) {
        Ok(_) => log::info!("Migrated database: added is_meta_conversation column"),
        Err(e) if e.to_string().contains("duplicate column name") => {}
        Err(e) => return Err(e.into()),
    }

    // Migration: Add repo_root column to conversations
    match conn.execute(
        "ALTER TABLE conversations ADD COLUMN repo_root TEXT",
        [],
    ) {
        Ok(_) => log::info!("Migrated database: added repo_root column"),
        Err(e) if e.to_string().contains("duplicate column name") => {}
        Err(e) => return Err(e.into()),
    }

    // Migration: Create repo_root_cache table
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS repo_root_cache (
            project_path TEXT PRIMARY KEY,
            repo_root TEXT,
            resolved_at TEXT DEFAULT CURRENT_TIMESTAMP
        )",
    )?;

    // Migration: Create index on repo_root
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_conv_repo_root ON conversations(repo_root)",
    )?;

    // Migration: Add source column to conversations
    match conn.execute(
        "ALTER TABLE conversations ADD COLUMN source TEXT DEFAULT 'claude_code'",
        [],
    ) {
        Ok(_) => log::info!("Migrated database: added source column"),
        Err(e) if e.to_string().contains("duplicate column name") => {}
        Err(e) => return Err(e.into()),
    }

    // Migration: Create index on source
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_conv_source ON conversations(source)",
    )?;

    Ok(())
}
