use rusqlite::{Connection, OptionalExtension};

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

    // Migration: Create claude_code_sync_state table
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS claude_code_sync_state (
            file_path TEXT PRIMARY KEY,
            mtime REAL NOT NULL,
            indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
        )",
    )?;

    // Migration: Switch FTS5 from unicode61 to trigram tokenizer for CJK support
    migrate_fts_to_trigram(conn)?;

    Ok(())
}

/// Migrate the FTS5 table from unicode61 to trigram tokenizer.
/// Only runs if the existing table does not already use trigram.
fn migrate_fts_to_trigram(conn: &Connection) -> Result<()> {
    // Check if FTS table already uses trigram
    let fts_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='message_content_fts'",
            [],
            |row| row.get(0),
        )
        .optional()?;

    let Some(sql) = fts_sql else {
        return Ok(()); // FTS table doesn't exist yet, schema.sql will create it
    };

    if sql.contains("trigram") {
        return Ok(()); // Already migrated
    }

    log::info!("Migrating FTS index to trigram tokenizer for Japanese/CJK support...");

    // Wrap in transaction to avoid partial state on failure
    let tx = conn.unchecked_transaction()?;

    // Drop triggers first
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_ai;")?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_ad;")?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_au;")?;

    // Drop old FTS table
    tx.execute_batch("DROP TABLE IF EXISTS message_content_fts;")?;

    // Recreate with trigram tokenizer
    tx.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS message_content_fts USING fts5(
            message_uuid UNINDEXED,
            full_content,
            content='messages',
            content_rowid='rowid',
            tokenize='trigram case_sensitive 0'
        );"
    )?;

    // Recreate triggers
    tx.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO message_content_fts(rowid, message_uuid, full_content)
            VALUES (new.rowid, new.message_uuid, new.full_content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
            DELETE FROM message_content_fts WHERE rowid = old.rowid;
        END;

        CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
            DELETE FROM message_content_fts WHERE rowid = old.rowid;
            INSERT INTO message_content_fts(rowid, message_uuid, full_content)
            VALUES (new.rowid, new.message_uuid, new.full_content);
        END;"
    )?;

    // Rebuild FTS index from existing data
    tx.execute(
        "INSERT INTO message_content_fts(message_content_fts) VALUES('rebuild')",
        [],
    )?;

    tx.commit()?;

    log::info!("FTS trigram migration complete.");
    Ok(())
}
