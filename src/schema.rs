use rusqlite::{Connection, OptionalExtension};

use crate::error::{AppError, Result};

const SCHEMA_SQL: &str = include_str!("../data/schema.sql");

/// Trigger definitions that keep `message_content_fts` in sync with `messages`.
///
/// Must stay identical to the copy in data/schema.sql, which is what fresh databases
/// get; `test_migration_triggers_match_schema_sql` enforces that. Migration 9 recreates
/// the delete/update triggers from here, since `CREATE TRIGGER IF NOT EXISTS` cannot
/// update an existing database.
pub const FTS_SYNC_TRIGGERS: &str = "
    CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO message_content_fts(rowid, message_uuid, full_content)
        VALUES (new.rowid, new.message_uuid, new.full_content);
    END;

    CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
        INSERT INTO message_content_fts(message_content_fts, rowid, message_uuid, full_content)
        VALUES ('delete', old.rowid, old.message_uuid, old.full_content);
    END;

    CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
        INSERT INTO message_content_fts(message_content_fts, rowid, message_uuid, full_content)
        VALUES ('delete', old.rowid, old.message_uuid, old.full_content);
        INSERT INTO message_content_fts(rowid, message_uuid, full_content)
        VALUES (new.rowid, new.message_uuid, new.full_content);
    END;";

/// Migration kinds.
enum MigrationKind {
    Sql(&'static str),
    Custom,
}

/// Migration definition: (version, description, kind).
const MIGRATIONS: &[(i64, &str, MigrationKind)] = &[
    (
        1,
        "add is_meta_conversation column",
        MigrationKind::Sql(
            "ALTER TABLE messages ADD COLUMN is_meta_conversation BOOLEAN DEFAULT FALSE",
        ),
    ),
    (
        2,
        "add repo_root column to conversations",
        MigrationKind::Sql("ALTER TABLE conversations ADD COLUMN repo_root TEXT"),
    ),
    (
        3,
        "create repo_root_cache table",
        MigrationKind::Sql(
            "CREATE TABLE IF NOT EXISTS repo_root_cache (
              project_path TEXT PRIMARY KEY,
              repo_root TEXT,
              resolved_at TEXT DEFAULT CURRENT_TIMESTAMP
          )",
        ),
    ),
    (
        4,
        "create index on repo_root",
        MigrationKind::Sql(
            "CREATE INDEX IF NOT EXISTS idx_conv_repo_root ON conversations(repo_root)",
        ),
    ),
    (
        5,
        "add source column to conversations",
        MigrationKind::Sql(
            "ALTER TABLE conversations ADD COLUMN source TEXT DEFAULT 'claude_code'",
        ),
    ),
    (
        6,
        "create index on source",
        MigrationKind::Sql("CREATE INDEX IF NOT EXISTS idx_conv_source ON conversations(source)"),
    ),
    (
        7,
        "create claude_code_sync_state table",
        MigrationKind::Sql(
            "CREATE TABLE IF NOT EXISTS claude_code_sync_state (
              file_path TEXT PRIMARY KEY,
              mtime REAL NOT NULL,
              indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
          )",
        ),
    ),
    (8, "migrate FTS to trigram tokenizer", MigrationKind::Custom),
    (
        9,
        "fix FTS delete/update triggers for external-content table",
        MigrationKind::Custom,
    ),
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
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))?;
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
            MigrationKind::Custom => detect_custom_migration_applied(conn, *version)
                .map_err(|e| migration_probe_error(conn, *version, &e))?,
        };

        if applied {
            record_migration(conn, *version)?;
        }
    }

    Ok(())
}

/// Turn a failed migration probe into something the reader can act on.
///
/// Without this the user gets a bare rusqlite message from a code path they have no way to
/// connect to their database, on a command (`search`, `list`) they did not know touched the
/// schema at all.
fn migration_probe_error(conn: &Connection, version: i64, source: &AppError) -> AppError {
    let path = conn.path().unwrap_or("<in-memory>");
    AppError::General(format!(
        "could not determine whether schema migration {} was applied to {}: {}. \
         Refusing to continue: assuming \"already applied\" would skip the migration \
         permanently. If the database is damaged, back it up and rebuild the index with \
         'ai-conversation-search init --force'.",
        version, path, source
    ))
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

/// Detect whether a custom migration's effect is already present.
///
/// Errors are propagated rather than read as "already applied". `true` here makes
/// `bootstrap_existing_db` record the version, after which `is_migration_applied` skips the
/// body forever -- so a transient SQLITE_BUSY or a corrupt page would permanently convince
/// the database it had been migrated. Note the asymmetry with `detect_sql_migration_applied`,
/// which defaults to `false` on a failed probe: there, a wrong answer just re-runs a
/// migration that then fails loudly. Only this side fails in the unsafe direction.
fn detect_custom_migration_applied(conn: &Connection, version: i64) -> Result<bool> {
    // (object type, object name, marker found only in the post-migration form)
    let (obj_type, obj_name, marker) = match version {
        8 => ("table", "message_content_fts", "trigram"),
        // The fixed trigger issues a 'delete' command; the broken one issued a
        // DELETE statement.
        9 => ("trigger", "messages_ad", "'delete'"),
        v => unreachable!("unhandled custom migration version: {}", v),
    };

    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
            rusqlite::params![obj_type, obj_name],
            // sqlite_master.sql is NULL for auto-created objects, which is legal and must
            // not become a hard error via InvalidColumnType.
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();

    // Absent means a fresh DB, where schema.sql already creates the correct form.
    Ok(sql.is_none_or(|s| s.contains(marker)))
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
    let table = after
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()?;
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
        9 => migrate_fix_fts_delete_triggers(conn),
        v => unreachable!("unhandled custom migration version: {}", v),
    }
}

/// Replace the FTS sync triggers with the external-content-safe form.
///
/// `CREATE TRIGGER IF NOT EXISTS` in schema.sql cannot update an existing database, so
/// the old definitions have to be dropped explicitly. Orphaned index entries left behind
/// by the previous triggers are not repaired here -- that needs a full rebuild, which
/// `prune-observer` performs.
fn migrate_fix_fts_delete_triggers(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_ad;")?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_au;")?;
    tx.execute_batch(FTS_SYNC_TRIGGERS)?;
    tx.commit()?;
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
        );",
    )?;

    tx.execute_batch(FTS_SYNC_TRIGGERS)?;

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

    fn insert_message(conn: &Connection, uuid: &str, content: &str) {
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, depth, timestamp, message_type, full_content) VALUES (?, 'sess1', 0, '2025-01-15T10:00:00', 'user', ?)",
            rusqlite::params![uuid, content],
        )
        .unwrap();
    }

    /// Counts index entries whose content row is gone. A plain
    /// `SELECT ... FROM message_content_fts WHERE full_content LIKE ...` cannot see these
    /// -- it reads through to the content table and errors on the missing row.
    fn orphan_fts_rows(conn: &Connection, needle: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM (SELECT rowid FROM message_content_fts WHERE message_content_fts MATCH ?) f
             LEFT JOIN messages m ON m.rowid = f.rowid WHERE m.rowid IS NULL",
            [needle],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn test_fts_delete_trigger_removes_index_entry() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        insert_message(&conn, "m1", "alphabet soup");

        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_content_fts WHERE message_content_fts MATCH 'alp'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 1);

        conn.execute("DELETE FROM messages WHERE message_uuid = 'm1'", [])
            .unwrap();

        assert_eq!(orphan_fts_rows(&conn, "alp"), 0);
    }

    #[test]
    fn test_fts_update_trigger_reindexes_content() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        insert_message(&conn, "m1", "alphabet soup");

        conn.execute(
            "UPDATE messages SET full_content = 'zebra crossing' WHERE message_uuid = 'm1'",
            [],
        )
        .unwrap();

        let stale: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_content_fts WHERE message_content_fts MATCH 'alp'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "old terms must not survive an update");

        let fresh: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_content_fts WHERE message_content_fts MATCH 'zeb'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fresh, 1);
    }

    /// `FTS_SYNC_TRIGGERS` is a second copy of what data/schema.sql defines, and only the
    /// migration path uses it -- a fresh database never exercises it, so drift between the
    /// two would go unnoticed until an upgraded database silently mis-indexed.
    #[test]
    fn test_migration_triggers_match_schema_sql() {
        for trigger in ["messages_ai", "messages_ad", "messages_au"] {
            let from_schema = setup_fresh_db();
            init_schema(&from_schema).unwrap();
            let expected: String = from_schema
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='trigger' AND name = ?",
                    [trigger],
                    |row| row.get(0),
                )
                .unwrap();

            let from_const = setup_fresh_db();
            init_schema(&from_const).unwrap();
            from_const
                .execute_batch(&format!("DROP TRIGGER {};", trigger))
                .unwrap();
            from_const.execute_batch(FTS_SYNC_TRIGGERS).unwrap();
            let actual: String = from_const
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='trigger' AND name = ?",
                    [trigger],
                    |row| row.get(0),
                )
                .unwrap();

            assert_eq!(
                normalize_sql(&actual),
                normalize_sql(&expected),
                "{} differs between FTS_SYNC_TRIGGERS and data/schema.sql",
                trigger
            );
        }
    }

    fn normalize_sql(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// A database still carrying the broken triggers must be repaired by migration 9.
    #[test]
    fn test_migration_9_replaces_legacy_delete_trigger() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();

        // Reinstate the pre-0.15.0 trigger and rewind the recorded version.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS messages_ad;
             CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                 DELETE FROM message_content_fts WHERE rowid = old.rowid;
             END;",
        )
        .unwrap();
        conn.execute("DELETE FROM schema_version WHERE version = 9", [])
            .unwrap();

        // Confirm the legacy trigger really does strand an entry, so passing after the
        // migration means something.
        insert_message(&conn, "legacy", "alphabet soup");
        conn.execute("DELETE FROM messages WHERE message_uuid = 'legacy'", [])
            .unwrap();
        assert_eq!(
            orphan_fts_rows(&conn, "alp"),
            1,
            "legacy trigger should leave the index entry behind"
        );

        init_schema(&conn).unwrap();

        insert_message(&conn, "m1", "zebra crossing");
        conn.execute("DELETE FROM messages WHERE message_uuid = 'm1'", [])
            .unwrap();
        assert_eq!(orphan_fts_rows(&conn, "zeb"), 0);
    }

    /// On a fresh database schema.sql already produces the post-migration form, so the
    /// detectors must report "applied" and let bootstrap record the versions without
    /// running the bodies.
    #[test]
    fn test_detect_custom_migration_fresh_db_reports_applied() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();

        assert!(detect_custom_migration_applied(&conn, 8).unwrap());
        assert!(detect_custom_migration_applied(&conn, 9).unwrap());
    }

    /// The detector has to recognise the pre-0.15.0 trigger as *not* migrated; if it
    /// reported "applied", bootstrap would record version 9 and the repair would never run.
    #[test]
    fn test_detect_custom_migration_legacy_trigger_reports_unapplied() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();

        conn.execute_batch(
            "DROP TRIGGER IF EXISTS messages_ad;
             CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                 DELETE FROM message_content_fts WHERE rowid = old.rowid;
             END;",
        )
        .unwrap();

        assert!(!detect_custom_migration_applied(&conn, 9).unwrap());
    }

    /// An absent object means a brand-new database, not a failed probe.
    #[test]
    fn test_detect_custom_migration_absent_object_reports_applied() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        conn.execute_batch("DROP TRIGGER IF EXISTS messages_ad;")
            .unwrap();

        assert!(detect_custom_migration_applied(&conn, 9).unwrap());
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
            );",
        )
        .unwrap();

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

    #[test]
    fn test_migration_versions_are_strictly_increasing() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|(v, _, _)| *v).collect();
        for window in versions.windows(2) {
            assert!(
                window[0] < window[1],
                "Migration versions must be strictly increasing: {} >= {}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn test_parse_alter_add_column() {
        let result = parse_alter_add_column(
            "ALTER TABLE messages ADD COLUMN is_meta_conversation BOOLEAN DEFAULT FALSE",
        );
        assert_eq!(
            result,
            Some(("messages".to_string(), "is_meta_conversation".to_string()))
        );

        let result = parse_alter_add_column("ALTER TABLE conversations ADD COLUMN repo_root TEXT");
        assert_eq!(
            result,
            Some(("conversations".to_string(), "repo_root".to_string()))
        );

        // Not an ALTER TABLE
        assert_eq!(parse_alter_add_column("CREATE TABLE foo (id INT)"), None);
    }

    #[test]
    fn test_parse_create_table() {
        let result = parse_create_table(
            "CREATE TABLE IF NOT EXISTS repo_root_cache (
                project_path TEXT PRIMARY KEY,
                repo_root TEXT
            )",
        );
        assert_eq!(result, Some("repo_root_cache".to_string()));

        let result = parse_create_table("CREATE TABLE foo (id INT)");
        assert_eq!(result, Some("foo".to_string()));

        assert_eq!(parse_create_table("ALTER TABLE foo ADD COLUMN bar"), None);
    }

    #[test]
    fn test_parse_create_index() {
        let result = parse_create_index(
            "CREATE INDEX IF NOT EXISTS idx_conv_repo_root ON conversations(repo_root)",
        );
        assert_eq!(result, Some("idx_conv_repo_root".to_string()));

        let result = parse_create_index("CREATE INDEX idx_foo ON bar(baz)");
        assert_eq!(result, Some("idx_foo".to_string()));

        assert_eq!(parse_create_index("ALTER TABLE foo ADD COLUMN bar"), None);
    }

    #[test]
    fn test_parse_all_migration_sql() {
        // Verify that all SQL migrations can be parsed by detect_sql_migration_applied
        for (version, _desc, kind) in MIGRATIONS {
            if let MigrationKind::Sql(sql) = kind {
                let detected = sql.contains("ALTER TABLE")
                    || sql.contains("CREATE TABLE")
                    || sql.contains("CREATE INDEX");
                assert!(
                    detected,
                    "Migration {} SQL should match at least one parser: {}",
                    version, sql
                );
            }
        }
    }
}
