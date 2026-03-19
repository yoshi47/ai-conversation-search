use rusqlite::{Connection, OptionalExtension};

use crate::error::Result;

const SCHEMA_SQL: &str = include_str!("../data/schema.sql");

/// Migration kinds.
enum MigrationKind {
    Sql(&'static str),
    Custom,
}

/// Migration definition: (version, description, kind).
const MIGRATIONS: &[(i64, &str, MigrationKind)] = &[
    (1, "add is_meta_conversation column",
     MigrationKind::Sql("ALTER TABLE messages ADD COLUMN is_meta_conversation BOOLEAN DEFAULT FALSE")),
    (2, "add repo_root column to conversations",
     MigrationKind::Sql("ALTER TABLE conversations ADD COLUMN repo_root TEXT")),
    (3, "create repo_root_cache table",
     MigrationKind::Sql(
         "CREATE TABLE IF NOT EXISTS repo_root_cache (
              project_path TEXT PRIMARY KEY,
              repo_root TEXT,
              resolved_at TEXT DEFAULT CURRENT_TIMESTAMP
          )")),
    (4, "create index on repo_root",
     MigrationKind::Sql("CREATE INDEX IF NOT EXISTS idx_conv_repo_root ON conversations(repo_root)")),
    (5, "add source column to conversations",
     MigrationKind::Sql("ALTER TABLE conversations ADD COLUMN source TEXT DEFAULT 'claude_code'")),
    (6, "create index on source",
     MigrationKind::Sql("CREATE INDEX IF NOT EXISTS idx_conv_source ON conversations(source)")),
    (7, "create claude_code_sync_state table",
     MigrationKind::Sql(
         "CREATE TABLE IF NOT EXISTS claude_code_sync_state (
              file_path TEXT PRIMARY KEY,
              mtime REAL NOT NULL,
              indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
          )")),
    (8, "migrate FTS to trigram tokenizer",
     MigrationKind::Custom),
];

/// Initialize the database schema and run migrations.
///
/// Order of operations:
/// 1. Create base tables (schema.sql) — uses IF NOT EXISTS, so safe for existing DBs.
///    For existing DBs, some indexes/triggers may fail if migration-added columns are
///    missing; those errors are harmless and will succeed after migrations run.
/// 2. Create schema_version table if needed.
/// 3. Bootstrap: detect already-applied migrations in existing DBs (no schema_version yet).
/// 4. Run any unapplied migrations (adding columns, tables, indexes, FTS changes).
/// 5. Re-run schema.sql to ensure all indexes/triggers exist (now that columns are present).
pub fn init_schema(conn: &Connection) -> Result<()> {
    // First pass: create base tables. Errors from indexes on missing columns are expected
    // for pre-migration databases and will be resolved after migrations run.
    let _ = conn.execute_batch(SCHEMA_SQL);

    ensure_schema_version_table(conn)?;
    bootstrap_existing_db(conn)?;

    for (version, description, kind) in MIGRATIONS {
        if is_migration_applied(conn, *version) {
            continue;
        }

        log::info!("Running migration {}: {}", version, description);

        match kind {
            MigrationKind::Sql(sql) => {
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(sql)?;
                record_migration(&tx, *version)?;
                tx.commit()?;
            }
            MigrationKind::Custom => {
                run_custom_migration(conn, *version)?;
                record_migration(conn, *version)?;
            }
        }
    }

    // Second pass: now that all migrations have run, ensure all indexes/triggers exist
    conn.execute_batch(SCHEMA_SQL)?;

    Ok(())
}

fn ensure_schema_version_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT DEFAULT CURRENT_TIMESTAMP
        )",
    )?;
    Ok(())
}

fn is_migration_applied(conn: &Connection, version: i64) -> bool {
    conn.query_row(
        "SELECT 1 FROM schema_version WHERE version = ?",
        [version],
        |_| Ok(()),
    )
    .is_ok()
}

fn record_migration(conn: &Connection, version: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO schema_version (version) VALUES (?)",
        [version],
    )?;
    Ok(())
}

/// For existing databases that predate schema_version tracking,
/// detect which migrations have already been applied and record them.
fn bootstrap_existing_db(conn: &Connection) -> Result<()> {
    // If any versions already recorded, bootstrap is done
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    if count > 0 {
        return Ok(());
    }

    // Check if this is a brand new database (no messages table data)
    // New DBs have schema_version created but no migrations recorded yet,
    // and schema.sql already includes all columns/tables.
    // We detect "existing DB" by checking if the messages table existed before
    // our schema.sql created it - but since schema.sql uses IF NOT EXISTS,
    // we check if a migration-specific artifact already exists.

    for (version, _description, kind) in MIGRATIONS {
        let applied = match kind {
            MigrationKind::Sql(sql) => detect_sql_migration_applied(conn, sql),
            MigrationKind::Custom => detect_custom_migration_applied(conn, *version),
        };

        if applied {
            record_migration(conn, *version)?;
        }
    }

    Ok(())
}

/// Detect if a SQL migration has already been applied by examining its effects.
fn detect_sql_migration_applied(conn: &Connection, sql: &str) -> bool {
    if sql.contains("ALTER TABLE") && sql.contains("ADD COLUMN") {
        // Extract table and column names
        if let Some((table, column)) = parse_alter_add_column(sql) {
            return column_exists(conn, &table, &column);
        }
    }
    if sql.contains("CREATE TABLE") {
        if let Some(table) = parse_create_table(sql) {
            return table_exists(conn, &table);
        }
    }
    if sql.contains("CREATE INDEX") {
        if let Some(index) = parse_create_index(sql) {
            return index_exists(conn, &index);
        }
    }
    false
}

fn detect_custom_migration_applied(conn: &Connection, version: i64) -> bool {
    match version {
        8 => {
            // FTS trigram migration: check if FTS table uses trigram
            let fts_sql: Option<String> = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name='message_content_fts'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .unwrap_or(None);
            fts_sql.map_or(true, |s| s.contains("trigram"))
        }
        _ => false,
    }
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        &format!(
            "SELECT 1 FROM pragma_table_info('{}') WHERE name = ?",
            table
        ),
        [column],
        |_| Ok(()),
    )
    .is_ok()
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

fn index_exists(conn: &Connection, index: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='index' AND name = ?",
        [index],
        |_| Ok(()),
    )
    .is_ok()
}

/// Parse "ALTER TABLE <table> ADD COLUMN <col> ..."
fn parse_alter_add_column(sql: &str) -> Option<(String, String)> {
    let upper = sql.to_uppercase();
    let alter_pos = upper.find("ALTER TABLE")?;
    let add_pos = upper.find("ADD COLUMN")?;

    let table_part = &sql[alter_pos + 11..add_pos].trim();
    let table = table_part.split_whitespace().next()?.to_string();

    let col_part = &sql[add_pos + 10..].trim();
    let column = col_part.split_whitespace().next()?.to_string();

    Some((table, column))
}

/// Parse "CREATE TABLE [IF NOT EXISTS] <table> ..."
fn parse_create_table(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    let create_pos = upper.find("CREATE TABLE")?;
    let after = &sql[create_pos + 12..].trim();
    let after = if after.to_uppercase().starts_with("IF NOT EXISTS") {
        &after[13..].trim()
    } else {
        after
    };
    let table = after.split(|c: char| c.is_whitespace() || c == '(').next()?;
    Some(table.to_string())
}

/// Parse "CREATE INDEX [IF NOT EXISTS] <index> ..."
fn parse_create_index(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    let create_pos = upper.find("CREATE INDEX")?;
    let after = &sql[create_pos + 12..].trim();
    let after = if after.to_uppercase().starts_with("IF NOT EXISTS") {
        &after[13..].trim()
    } else {
        after
    };
    let index = after.split_whitespace().next()?;
    Some(index.to_string())
}

/// Run custom migration by version.
fn run_custom_migration(conn: &Connection, version: i64) -> Result<()> {
    match version {
        8 => migrate_fts_to_trigram(conn),
        _ => Ok(()),
    }
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

    let tx = conn.unchecked_transaction()?;

    tx.execute_batch("DROP TRIGGER IF EXISTS messages_ai;")?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_ad;")?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_au;")?;
    tx.execute_batch("DROP TABLE IF EXISTS message_content_fts;")?;

    tx.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS message_content_fts USING fts5(
            message_uuid UNINDEXED,
            full_content,
            content='messages',
            content_rowid='rowid',
            tokenize='trigram case_sensitive 0'
        );"
    )?;

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

    tx.execute(
        "INSERT INTO message_content_fts(message_content_fts) VALUES('rebuild')",
        [],
    )?;

    tx.commit()?;

    log::info!("FTS trigram migration complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn
    }

    #[test]
    fn test_new_db_schema_init() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();

        // Verify schema_version has all migrations
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, MIGRATIONS.len() as i64);

        // Verify key tables exist
        assert!(table_exists(&conn, "messages"));
        assert!(table_exists(&conn, "conversations"));
        assert!(table_exists(&conn, "repo_root_cache"));
        assert!(table_exists(&conn, "claude_code_sync_state"));
        assert!(table_exists(&conn, "schema_version"));

        // Verify key columns
        assert!(column_exists(&conn, "messages", "is_meta_conversation"));
        assert!(column_exists(&conn, "conversations", "repo_root"));
        assert!(column_exists(&conn, "conversations", "source"));
    }

    #[test]
    fn test_idempotent_init_schema() {
        let conn = setup_fresh_db();

        // Run twice
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, MIGRATIONS.len() as i64);
    }

    #[test]
    fn test_existing_db_bootstrap() {
        let conn = setup_fresh_db();

        // Simulate an existing DB: run schema.sql (which includes all columns)
        // but without schema_version table
        conn.execute_batch(SCHEMA_SQL).unwrap();

        // No schema_version table yet
        assert!(!table_exists(&conn, "schema_version"));

        // Now run init_schema - it should bootstrap
        init_schema(&conn).unwrap();

        // All migrations should be recorded as applied
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, MIGRATIONS.len() as i64);
    }

    #[test]
    fn test_partial_migration_bootstrap() {
        let conn = setup_fresh_db();

        // Create base schema without some columns
        conn.execute_batch(
            "CREATE TABLE messages (
                message_uuid TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                parent_uuid TEXT,
                is_sidechain BOOLEAN DEFAULT FALSE,
                depth INTEGER DEFAULT 0,
                timestamp TEXT NOT NULL,
                message_type TEXT NOT NULL,
                project_path TEXT,
                conversation_file TEXT,
                summary TEXT,
                full_content TEXT NOT NULL,
                is_summarized BOOLEAN DEFAULT FALSE,
                is_tool_noise BOOLEAN DEFAULT FALSE,
                summary_method TEXT,
                indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE conversations (
                session_id TEXT PRIMARY KEY,
                project_path TEXT,
                conversation_file TEXT,
                root_message_uuid TEXT,
                leaf_message_uuid TEXT,
                conversation_summary TEXT,
                first_message_at TEXT,
                last_message_at TEXT,
                message_count INTEGER DEFAULT 0,
                indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
            );
            CREATE VIRTUAL TABLE message_content_fts USING fts5(
                message_uuid UNINDEXED,
                full_content,
                content='messages',
                content_rowid='rowid',
                tokenize='unicode61'
            );"
        ).unwrap();

        // Missing: is_meta_conversation, repo_root, repo_root_cache table,
        //          idx_conv_repo_root, source, idx_conv_source, claude_code_sync_state,
        //          FTS trigram

        init_schema(&conn).unwrap();

        // Verify missing items were added
        assert!(column_exists(&conn, "messages", "is_meta_conversation"));
        assert!(column_exists(&conn, "conversations", "repo_root"));
        assert!(column_exists(&conn, "conversations", "source"));
        assert!(table_exists(&conn, "repo_root_cache"));
        assert!(table_exists(&conn, "claude_code_sync_state"));
        assert!(index_exists(&conn, "idx_conv_repo_root"));
        assert!(index_exists(&conn, "idx_conv_source"));

        // Verify FTS uses trigram
        let fts_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='message_content_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fts_sql.contains("trigram"));

        // All migrations recorded
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, MIGRATIONS.len() as i64);
    }

    #[test]
    fn test_schema_version_contents() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();

        // Verify each version is recorded
        for (version, _, _) in MIGRATIONS {
            assert!(
                is_migration_applied(&conn, *version),
                "migration {} should be recorded",
                version
            );
        }
    }
}
