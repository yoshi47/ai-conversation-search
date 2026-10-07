use rusqlite::{Connection, OptionalExtension};

use crate::error::{AppError, Result};

const SCHEMA_SQL: &str = include_str!("../data/schema.sql");

/// Trigger definitions that keep `message_content_fts` and
/// `message_content_bigram_fts` in sync with `messages`.
///
/// Must stay identical to the copy in data/schema.sql, which is what fresh databases
/// get; `test_migration_triggers_match_schema_sql` enforces that. Migrations 9, 10 and 16,
/// the trigger repair in `init_schema` and prune-observer recreate the triggers from here,
/// since `CREATE TRIGGER IF NOT EXISTS` cannot update an existing database.
pub const FTS_SYNC_TRIGGERS: &str = "
    CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO message_content_fts(rowid, message_uuid, full_content)
        VALUES (new.rowid, new.message_uuid, new.full_content);
        INSERT OR IGNORE INTO message_bigram_pending(msg_rowid) VALUES (new.rowid);
    END;

    CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
        INSERT INTO message_content_fts(message_content_fts, rowid, message_uuid, full_content)
        VALUES ('delete', old.rowid, old.message_uuid, old.full_content);
        DELETE FROM message_content_bigram_fts WHERE rowid = old.rowid;
        DELETE FROM message_bigram_pending WHERE msg_rowid = old.rowid;
    END;

    CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE OF full_content, message_uuid ON messages BEGIN
        INSERT INTO message_content_fts(message_content_fts, rowid, message_uuid, full_content)
        VALUES ('delete', old.rowid, old.message_uuid, old.full_content);
        INSERT INTO message_content_fts(rowid, message_uuid, full_content)
        VALUES (new.rowid, new.message_uuid, new.full_content);
        DELETE FROM message_content_bigram_fts WHERE rowid = old.rowid;
        INSERT OR IGNORE INTO message_bigram_pending(msg_rowid) VALUES (new.rowid);
    END;";

/// Oldest schema a writer must understand, stored in `PRAGMA user_version`.
///
/// Why not refuse whenever the DB has a migration this binary does not know: most
/// migrations (new columns, indexes) leave older writers working, and refusing on them
/// would break every session that outlives a plugin update. Raise this only for a
/// migration that makes writes from older binaries unsafe, as migration 10 did.
/// 16 is the first schema whose writers check it; binaries before that ignore
/// `user_version`, so today it gates nothing.
const MIN_WRITER_SCHEMA: i64 = 16;

/// Rows analyzed per transaction by `drain_bigram_pending`, so a long backlog never
/// holds the write lock for the whole run.
const BIGRAM_DRAIN_BATCH: usize = 2000;

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
    (
        10,
        "add bigram FTS table for short-term/CJK search",
        MigrationKind::Custom,
    ),
    (
        11,
        "add model column to messages",
        MigrationKind::Sql("ALTER TABLE messages ADD COLUMN model TEXT"),
    ),
    (
        12,
        "add model column to conversations",
        MigrationKind::Sql("ALTER TABLE conversations ADD COLUMN model TEXT"),
    ),
    (
        13,
        "create index on conversations(model)",
        MigrationKind::Sql("CREATE INDEX IF NOT EXISTS idx_conv_model ON conversations(model)"),
    ),
    (
        14,
        "create index on messages(model)",
        MigrationKind::Sql("CREATE INDEX IF NOT EXISTS idx_msg_model ON messages(model)"),
    ),
    (
        15,
        "clear sync state to backfill models",
        MigrationKind::Custom,
    ),
    (
        16,
        "queue bigram indexing instead of calling bigram_analyze in triggers",
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
/// 6. Repair FTS triggers an older binary reverted, then raise `user_version`.
///
/// Before step 1, refuse a DB whose `user_version` requires a newer writer.
pub fn init_schema(conn: &Connection) -> Result<()> {
    // For `drain_bigram_pending` / `fill_bigram_missing` on connections that did not
    // come from `db::connect` (tests, in-memory). Registering twice is harmless.
    crate::bigram::register_sql_function(conn)?;

    // Before the first write below: the schema.sql pass already writes.
    let required_writer: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let latest = latest_known_migration();
    if required_writer > latest {
        return Err(AppError::General(format!(
            "the index at {} needs a newer ai-conversation-search (schema {} or later; \
             this binary knows up to {}). Update the plugin or the wrapper, then restart the session.",
            conn.path().unwrap_or("<in-memory>"),
            required_writer,
            latest
        )));
    }

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

    // An older binary's prune-observer recreates `messages_ad` without the queue cleanup.
    // Migration 16 is already recorded and will not rerun, so check the shape every time.
    if !fts_triggers_use_pending_queue(conn)? {
        eprintln!("Repairing FTS sync triggers reverted by an older ai-conversation-search");
        migrate_bigram_pending_queue(conn)?;
    }

    if required_writer < MIN_WRITER_SCHEMA {
        conn.execute_batch(&format!("PRAGMA user_version = {};", MIN_WRITER_SCHEMA))?;
    }

    Ok(())
}

fn latest_known_migration() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.0)
}

fn fts_triggers_use_pending_queue(conn: &Connection) -> Result<bool> {
    let current: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger'
         AND name IN ('messages_ai', 'messages_ad', 'messages_au')
         AND sql LIKE '%message_bigram_pending%'",
        [],
        |r| r.get(0),
    )?;
    Ok(current == 3)
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
/// migration that is either idempotent (`CREATE ... IF NOT EXISTS`) or fails loudly
/// (`ALTER TABLE ADD COLUMN`). Only this side fails in the unsafe direction.
fn detect_custom_migration_applied(conn: &Connection, version: i64) -> Result<bool> {
    // (object type, object name, marker found only in the post-migration form)
    let (obj_type, obj_name, marker) = match version {
        8 => ("table", "message_content_fts", "trigram"),
        // The fixed trigger issues a 'delete' command; the broken one issued a
        // DELETE statement.
        9 => ("trigger", "messages_ad", "'delete'"),
        // The bigram triggers feed `message_content_bigram_fts`; the pre-10
        // `messages_ai` body only touches `message_content_fts`.
        10 => ("trigger", "messages_ai", "bigram"),
        // Pre-16 `messages_ai` called `bigram_analyze` instead of queueing.
        16 => ("trigger", "messages_ai", "message_bigram_pending"),
        // Data-only migration: a fresh DB has empty (or absent) sync tables,
        // so there is nothing to backfill and it counts as applied.
        15 => return Ok(sync_tables_empty_for_model_backfill(conn)),
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

/// Whether the model-backfill migration has nothing to do.
///
/// Returns true when no sync table holds rows: a fresh DB (nothing indexed yet)
/// or a DB that already ran the backfill and has not indexed since. An existing
/// DB with index history reports false so the migration runs once and forces a
/// reparse that fills `messages.model` / `conversations.model`.
fn sync_tables_empty_for_model_backfill(conn: &Connection) -> bool {
    for table in [
        "claude_code_sync_state",
        "codex_sync_state",
        "opencode_sync_state",
    ] {
        if !table_exists(conn, table) {
            continue;
        }
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
        if count > 0 {
            return false;
        }
    }
    true
}

/// Clear per-source sync cursors so the next index reparses every transcript.
///
/// Model columns (migrations 11-12) are NULL for rows indexed before they
/// existed, and the incremental paths skip unchanged files/sessions. Wiping the
/// cursors forces one full reparse; the indexers backfill NULL models in place
/// (Claude Code updates rows, OpenCode/Codex re-insert sessions), so no data
/// is lost. Runs inside its own transaction; a fresh DB has empty tables and
/// this is a no-op.
fn clear_sync_state_for_model_backfill(conn: &Connection) -> Result<()> {
    for table in [
        "claude_code_sync_state",
        "codex_sync_state",
        "opencode_sync_state",
    ] {
        if table_exists(conn, table) {
            conn.execute(&format!("DELETE FROM {}", table), [])?;
        }
    }
    Ok(())
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

/// How many `messages` rows have no entry in the bigram FTS table.
///
/// Non-zero on databases whose rows predate migration 10 (or whose backfill
/// was interrupted). Re-running the fill is safe: it only touches missing rows.
pub fn count_bigram_missing(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM messages m
         LEFT JOIN message_content_bigram_fts b ON b.rowid = m.rowid
         WHERE b.rowid IS NULL",
        [],
        |row| row.get(0),
    )?)
}

/// Fill bigram entries for rows counted by `count_bigram_missing`.
///
/// Single statement, idempotent, safe to re-run (interrupted runs just leave
/// rows for the next run). `OR REPLACE` tolerates a concurrent indexer
/// inserting the same rowid mid-statement. Returns the number of rows filled.
pub fn fill_bigram_missing(conn: &Connection) -> Result<usize> {
    let filled = conn.execute(
        "INSERT OR REPLACE INTO message_content_bigram_fts(rowid, message_uuid, bigram_content)
         SELECT m.rowid, m.message_uuid, bigram_analyze(m.full_content) FROM messages m
         LEFT JOIN message_content_bigram_fts b ON b.rowid = m.rowid
         WHERE b.rowid IS NULL",
        [],
    )?;
    Ok(filled)
}

/// Build bigram entries for rows queued by the FTS sync triggers.
///
/// Runs in batches of `BIGRAM_DRAIN_BATCH`, one IMMEDIATE transaction each, so a
/// long backlog never blocks other writers for the whole run; each batch removes
/// only the queue rows it read. Queue rows whose message is gone (deleted with the trigger
/// dropped, as prune-observer does) drop out of the JOIN and are just cleared.
/// Returns the number of bigram entries written.
pub fn drain_bigram_pending(conn: &Connection) -> Result<usize> {
    let mut filled = 0;
    loop {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(&format!(
            "DROP TABLE IF EXISTS temp.bigram_batch;
             CREATE TEMP TABLE bigram_batch AS
             SELECT msg_rowid FROM message_bigram_pending LIMIT {};",
            BIGRAM_DRAIN_BATCH
        ))?;
        filled += tx.execute(
            "INSERT OR REPLACE INTO message_content_bigram_fts(rowid, message_uuid, bigram_content)
             SELECT m.rowid, m.message_uuid, bigram_analyze(m.full_content)
             FROM temp.bigram_batch b JOIN messages m ON m.rowid = b.msg_rowid",
            [],
        )?;
        let batch = tx.execute(
            "DELETE FROM message_bigram_pending
             WHERE msg_rowid IN (SELECT msg_rowid FROM temp.bigram_batch)",
            [],
        )?;
        tx.execute_batch("DROP TABLE temp.bigram_batch;")?;
        tx.commit()?;
        if batch < BIGRAM_DRAIN_BATCH {
            return Ok(filled);
        }
    }
}

/// Run custom migration by version.
fn run_custom_migration(conn: &Connection, version: i64) -> Result<()> {
    match version {
        8 => migrate_fts_to_trigram(conn),
        9 => migrate_fix_fts_delete_triggers(conn),
        10 => migrate_add_bigram_fts(conn),
        15 => clear_sync_state_for_model_backfill(conn),
        16 => migrate_bigram_pending_queue(conn),
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

/// Add the bigram FTS table and extend the sync triggers to feed both tables.
///
/// `CREATE TRIGGER IF NOT EXISTS` in schema.sql cannot update an existing
/// database, so the old single-table definitions are dropped explicitly.
/// Existing message rows are NOT backfilled here: that needs one
/// `bigram_analyze` call per row and belongs in the `backfill-bigram` command,
/// not in a migration that runs inside the background indexer. New writes are
/// covered by the triggers from this point on.
fn migrate_add_bigram_fts(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS message_content_bigram_fts USING fts5(
            message_uuid UNINDEXED,
            bigram_content,
            tokenize='unicode61 remove_diacritics 1'
        );",
    )?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_ai;")?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_ad;")?;
    tx.execute_batch("DROP TRIGGER IF EXISTS messages_au;")?;
    tx.execute_batch(FTS_SYNC_TRIGGERS)?;
    tx.commit()?;
    Ok(())
}

/// Make the FTS sync triggers pure SQL, queueing bigram work in `message_bigram_pending`.
///
/// No rows are queued here: under the pre-16 triggers a write either filled its bigram
/// entry or failed and rolled back as a whole. Rows older than migration 10 remain
/// `backfill-bigram`'s job.
fn migrate_bigram_pending_queue(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_bigram_pending (msg_rowid INTEGER PRIMARY KEY);
         DROP TRIGGER IF EXISTS messages_ai;
         DROP TRIGGER IF EXISTS messages_ad;
         DROP TRIGGER IF EXISTS messages_au;",
    )?;
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
    ///
    /// Simulates a true pre-10 database: single-table `messages_ai`, legacy
    /// `messages_ad`, and no bigram table at all. Rewinding both 9 and 10
    /// exercises the migration chain end to end.
    #[test]
    fn test_migration_9_replaces_legacy_delete_trigger() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();

        // Reinstate the pre-bigram/-0.15.0 triggers and rewind the versions.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS messages_ai;
             CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                 INSERT INTO message_content_fts(rowid, message_uuid, full_content)
                 VALUES (new.rowid, new.message_uuid, new.full_content);
             END;
             DROP TRIGGER IF EXISTS messages_ad;
             CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                 DELETE FROM message_content_fts WHERE rowid = old.rowid;
             END;
             DROP TABLE IF EXISTS message_content_bigram_fts;",
        )
        .unwrap();
        conn.execute("DELETE FROM schema_version WHERE version IN (9, 10)", [])
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
        drain_bigram_pending(&conn).unwrap();
        assert_eq!(
            bigram_hits(&conn, "zebra"),
            1,
            "migration 10 must restore bigram writes"
        );
        conn.execute("DELETE FROM messages WHERE message_uuid = 'm1'", [])
            .unwrap();
        assert_eq!(orphan_fts_rows(&conn, "zeb"), 0);
        assert_eq!(orphan_bigram_rows(&conn, "zebra"), 0);
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
        assert!(detect_custom_migration_applied(&conn, 10).unwrap());
        assert!(detect_custom_migration_applied(&conn, 15).unwrap());
        assert!(detect_custom_migration_applied(&conn, 16).unwrap());
    }

    /// Counts bigram index entries whose content row is gone. Same shape as
    /// `orphan_fts_rows`, but the MATCH fragment must be bigram-analyzed
    /// (`analyze_for_query`), since the bigram table uses `unicode61`.
    fn orphan_bigram_rows(conn: &Connection, query_term: &str) -> i64 {
        let frag = crate::bigram::analyze_for_query(query_term).expect("test term must be FTSable");
        conn.query_row(
            "SELECT COUNT(*) FROM (SELECT rowid FROM message_content_bigram_fts WHERE message_content_bigram_fts MATCH ?) f
             LEFT JOIN messages m ON m.rowid = f.rowid WHERE m.rowid IS NULL",
            [frag],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn bigram_hits(conn: &Connection, query_term: &str) -> i64 {
        let frag = crate::bigram::analyze_for_query(query_term).expect("test term must be FTSable");
        conn.query_row(
            "SELECT COUNT(*) FROM message_content_bigram_fts WHERE message_content_bigram_fts MATCH ?",
            [frag],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// A database still carrying the pre-10 single-table triggers must be
    /// repaired by migration 10: new writes have to reach the bigram table.
    #[test]
    fn test_migration_10_replaces_legacy_triggers() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();

        // Reinstate the pre-bigram trigger and rewind the recorded version.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS messages_ai;
             CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                 INSERT INTO message_content_fts(rowid, message_uuid, full_content)
                 VALUES (new.rowid, new.message_uuid, new.full_content);
             END;",
        )
        .unwrap();
        conn.execute("DELETE FROM schema_version WHERE version = 10", [])
            .unwrap();
        assert!(!detect_custom_migration_applied(&conn, 10).unwrap());

        init_schema(&conn).unwrap();

        insert_message(&conn, "m1", "alphabet soup");
        drain_bigram_pending(&conn).unwrap();
        assert_eq!(
            bigram_hits(&conn, "alphabet"),
            1,
            "post-migration inserts must reach the bigram table"
        );
    }

    #[test]
    fn test_backfill_bigram_count_and_fill() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        insert_message(&conn, "m1", "alphabet soup");
        insert_message(&conn, "m2", "認証に失敗した");

        // Simulate a pre-10 database: rows exist but the bigram table is empty.
        conn.execute("DELETE FROM message_content_bigram_fts", [])
            .unwrap();
        assert_eq!(count_bigram_missing(&conn).unwrap(), 2);

        let filled = fill_bigram_missing(&conn).unwrap();
        assert_eq!(filled, 2);
        assert_eq!(count_bigram_missing(&conn).unwrap(), 0);
        assert_eq!(bigram_hits(&conn, "alphabet"), 1);
        assert_eq!(bigram_hits(&conn, "失敗"), 1);

        // Idempotent: a second run fills nothing.
        assert_eq!(fill_bigram_missing(&conn).unwrap(), 0);
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

    fn pending_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM message_bigram_pending", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// A migrated file DB plus a second connection that, like a pre-0.18 binary,
    /// never registers `bigram_analyze`.
    fn migrated_db_and_old_writer() -> (tempfile::TempDir, Connection, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let new_conn = Connection::open(&path).unwrap();
        init_schema(&new_conn).unwrap();
        let old_conn = Connection::open(&path).unwrap();
        (dir, new_conn, old_conn)
    }

    #[test]
    fn test_writer_without_bigram_function_can_insert_and_update() {
        let (_dir, _new_conn, old_conn) = migrated_db_and_old_writer();

        insert_message(&old_conn, "m1", "alphabet soup");
        old_conn
            .execute(
                "UPDATE messages SET full_content = 'mo 認証に失敗した' WHERE message_uuid = 'm1'",
                [],
            )
            .unwrap();
        old_conn
            .execute("DELETE FROM messages WHERE message_uuid = 'm1'", [])
            .unwrap();
    }

    #[test]
    fn test_drain_makes_rows_from_old_writer_searchable() {
        let (_dir, new_conn, old_conn) = migrated_db_and_old_writer();
        insert_message(&old_conn, "m1", "mo 認証に失敗した");
        assert_eq!(bigram_hits(&new_conn, "失敗"), 0);
        assert_eq!(pending_count(&new_conn), 1);

        assert_eq!(drain_bigram_pending(&new_conn).unwrap(), 1);

        assert_eq!(bigram_hits(&new_conn, "失敗"), 1);
        assert_eq!(bigram_hits(&new_conn, "mo"), 1);
        assert_eq!(pending_count(&new_conn), 0);
        assert_eq!(drain_bigram_pending(&new_conn).unwrap(), 0);
    }

    #[test]
    fn test_drain_processes_queues_larger_than_one_batch() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        let n = BIGRAM_DRAIN_BATCH + 3;
        for i in 0..n {
            insert_message(&conn, &format!("m{}", i), "失敗");
        }
        assert_eq!(drain_bigram_pending(&conn).unwrap(), n);
        assert_eq!(bigram_hits(&conn, "失敗"), n as i64);
        assert_eq!(pending_count(&conn), 0);
    }

    #[test]
    fn test_update_removes_old_terms_from_both_fts_before_drain() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        insert_message(&conn, "m1", "alphabet soup 失敗");
        drain_bigram_pending(&conn).unwrap();

        conn.execute(
            "UPDATE messages SET full_content = 'zebra crossing' WHERE message_uuid = 'm1'",
            [],
        )
        .unwrap();

        assert_eq!(bigram_hits(&conn, "alphabet"), 0);
        assert_eq!(bigram_hits(&conn, "失敗"), 0);
        let stale_trigram: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_content_fts WHERE message_content_fts MATCH 'alp'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_trigram, 0);

        drain_bigram_pending(&conn).unwrap();
        assert_eq!(bigram_hits(&conn, "zebra"), 1);
    }

    #[test]
    fn test_model_only_update_keeps_bigram_and_queues_nothing() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        insert_message(&conn, "m1", "認証に失敗した");
        drain_bigram_pending(&conn).unwrap();

        conn.execute(
            "UPDATE messages SET model = 'claude-x' WHERE message_uuid = 'm1'",
            [],
        )
        .unwrap();

        assert_eq!(bigram_hits(&conn, "失敗"), 1);
        assert_eq!(pending_count(&conn), 0);
    }

    #[test]
    fn test_delete_before_drain_leaves_no_pending_or_bigram_row() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        insert_message(&conn, "m1", "失敗");
        conn.execute("DELETE FROM messages WHERE message_uuid = 'm1'", [])
            .unwrap();

        assert_eq!(pending_count(&conn), 0);
        drain_bigram_pending(&conn).unwrap();
        assert_eq!(bigram_hits(&conn, "失敗"), 0);
    }

    /// v0.18.0 shape: triggers call `bigram_analyze`, no queue table, version 16 unrecorded.
    #[test]
    fn test_migration_16_from_v10_db_lets_old_writer_insert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let new_conn = Connection::open(&path).unwrap();
        init_schema(&new_conn).unwrap();
        new_conn
            .execute_batch(
                "DROP TRIGGER messages_ai; DROP TRIGGER messages_ad; DROP TRIGGER messages_au;
                 DROP TABLE message_bigram_pending;
                 CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                     INSERT INTO message_content_fts(rowid, message_uuid, full_content)
                     VALUES (new.rowid, new.message_uuid, new.full_content);
                     INSERT INTO message_content_bigram_fts(rowid, message_uuid, bigram_content)
                     VALUES (new.rowid, new.message_uuid, bigram_analyze(new.full_content));
                 END;
                 CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                     INSERT INTO message_content_fts(message_content_fts, rowid, message_uuid, full_content)
                     VALUES ('delete', old.rowid, old.message_uuid, old.full_content);
                     DELETE FROM message_content_bigram_fts WHERE rowid = old.rowid;
                 END;
                 CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
                     INSERT INTO message_content_fts(message_content_fts, rowid, message_uuid, full_content)
                     VALUES ('delete', old.rowid, old.message_uuid, old.full_content);
                     INSERT INTO message_content_fts(rowid, message_uuid, full_content)
                     VALUES (new.rowid, new.message_uuid, new.full_content);
                     DELETE FROM message_content_bigram_fts WHERE rowid = old.rowid;
                     INSERT INTO message_content_bigram_fts(rowid, message_uuid, bigram_content)
                     VALUES (new.rowid, new.message_uuid, bigram_analyze(new.full_content));
                 END;
                 DELETE FROM schema_version WHERE version = 16;
                 PRAGMA user_version = 0;",
            )
            .unwrap();
        insert_message(&new_conn, "v10", "既存の失敗");
        let doomed_writer = Connection::open(&path).unwrap();
        assert!(
            doomed_writer
                .execute(
                    "INSERT INTO messages (message_uuid, session_id, depth, timestamp, message_type, full_content) VALUES ('x', 's', 0, 't', 'user', 'x')",
                    [],
                )
                .is_err(),
            "the v10 trigger should reject a writer without bigram_analyze"
        );
        assert!(!detect_custom_migration_applied(&new_conn, 16).unwrap());

        init_schema(&new_conn).unwrap();

        // A fresh connection, as each index run is a new process: one whose prepare
        // already failed keeps its cached schema until something makes it re-read.
        let old_conn = Connection::open(&path).unwrap();
        insert_message(&old_conn, "m1", "新しい失敗");
        drain_bigram_pending(&new_conn).unwrap();
        assert_eq!(
            bigram_hits(&new_conn, "失敗"),
            2,
            "v10 entry kept, new row added"
        );
    }

    /// Pre-fix prune-observer recreates `messages_ad` from its own constant.
    #[test]
    fn test_init_schema_repairs_trigger_reverted_by_old_binary() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        conn.execute_batch(
            "DROP TRIGGER messages_ad;
             CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                 INSERT INTO message_content_fts(message_content_fts, rowid, message_uuid, full_content)
                 VALUES ('delete', old.rowid, old.message_uuid, old.full_content);
             END;",
        )
        .unwrap();

        init_schema(&conn).unwrap();

        insert_message(&conn, "m1", "失敗");
        conn.execute("DELETE FROM messages WHERE message_uuid = 'm1'", [])
            .unwrap();
        assert_eq!(pending_count(&conn), 0);
    }

    #[test]
    fn test_db_requiring_newer_writer_is_rejected_with_update_hint() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        conn.execute_batch("PRAGMA user_version = 9999;").unwrap();

        let err = init_schema(&conn).unwrap_err().to_string();
        assert!(err.contains("newer ai-conversation-search"), "{}", err);
    }

    #[test]
    fn test_init_schema_records_min_writer_schema() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        let user_version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(user_version, MIN_WRITER_SCHEMA);
        // "Never lowered" is untestable while MIN_WRITER_SCHEMA is the latest
        // migration: any larger value is rejected by the gate.
    }

    /// `backfill-bigram` fills rows still in the queue; the next drain re-analyzes them.
    #[test]
    fn test_drain_tolerates_rows_already_filled_by_backfill() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        insert_message(&conn, "m1", "失敗");
        fill_bigram_missing(&conn).unwrap();

        drain_bigram_pending(&conn).unwrap();

        assert_eq!(pending_count(&conn), 0);
        assert_eq!(bigram_hits(&conn, "失敗"), 1);
    }

    /// prune-observer deletes with `messages_ad` dropped, leaving queue rows behind.
    #[test]
    fn test_drain_clears_queue_rows_whose_message_is_gone() {
        let conn = setup_fresh_db();
        init_schema(&conn).unwrap();
        insert_message(&conn, "m1", "失敗");
        conn.execute_batch(
            "DROP TRIGGER messages_ad; DELETE FROM messages WHERE message_uuid = 'm1';",
        )
        .unwrap();
        conn.execute_batch(FTS_SYNC_TRIGGERS).unwrap();
        assert_eq!(pending_count(&conn), 1);

        assert_eq!(drain_bigram_pending(&conn).unwrap(), 0);
        assert_eq!(pending_count(&conn), 0);
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
        assert!(column_exists(&conn, "messages", "model"));
        assert!(column_exists(&conn, "conversations", "repo_root"));
        assert!(column_exists(&conn, "conversations", "source"));
        assert!(column_exists(&conn, "conversations", "model"));
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

    /// An index built before model tracking has rows but no model columns.
    /// Migrating must add the columns AND clear the sync cursors so the next
    /// index reparses transcripts and backfills the models. Existing message
    /// content must survive.
    #[test]
    fn test_model_migrations_add_columns_and_clear_sync_state() {
        let conn = setup_fresh_db();
        // Pre-model database at migration 10, built by hand so the model
        // columns are truly absent (SCHEMA_SQL already contains them).
        // init_schema's first SCHEMA_SQL pass fills in the FTS tables,
        // triggers and any missing sync tables around these.
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
                 is_meta_conversation BOOLEAN DEFAULT FALSE,
                 summary_method TEXT,
                 indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE conversations (
                 session_id TEXT PRIMARY KEY,
                 project_path TEXT,
                 repo_root TEXT,
                 conversation_file TEXT,
                 root_message_uuid TEXT,
                 leaf_message_uuid TEXT,
                 conversation_summary TEXT,
                 first_message_at TEXT,
                 last_message_at TEXT,
                 message_count INTEGER DEFAULT 0,
                 source TEXT DEFAULT 'claude_code',
                 indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE claude_code_sync_state (file_path TEXT PRIMARY KEY, mtime REAL NOT NULL);
             CREATE TABLE codex_sync_state (file_path TEXT PRIMARY KEY, mtime REAL NOT NULL);
             CREATE TABLE opencode_sync_state (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             INSERT INTO schema_version (version) VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10);
             INSERT INTO messages (message_uuid, session_id, timestamp, message_type, full_content)
               VALUES ('m1', 'sess1', '2025-01-15T10:00:00', 'assistant', 'hello');
             INSERT INTO conversations (session_id, message_count)
               VALUES ('sess1', 1);
             INSERT INTO claude_code_sync_state (file_path, mtime) VALUES ('/tmp/x.jsonl', 1.0);
             INSERT INTO codex_sync_state (file_path, mtime) VALUES ('/tmp/y.jsonl', 2.0);
             INSERT INTO opencode_sync_state (key, value) VALUES ('last_sync_time', '123');",
        )
        .unwrap();
        assert!(!column_exists(&conn, "messages", "model"));

        init_schema(&conn).unwrap();

        assert!(column_exists(&conn, "messages", "model"));
        assert!(column_exists(&conn, "conversations", "model"));
        assert!(index_exists(&conn, "idx_conv_model"));
        assert!(index_exists(&conn, "idx_msg_model"));
        assert!(is_migration_applied(&conn, 11));
        assert!(is_migration_applied(&conn, 15));
        // Content preserved.
        let content: String = conn
            .query_row(
                "SELECT full_content FROM messages WHERE message_uuid = 'm1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(content, "hello");
        // Sync cursors cleared so the next index reparses everything.
        for table in [
            "claude_code_sync_state",
            "codex_sync_state",
            "opencode_sync_state",
        ] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(n, 0, "{} should be cleared", table);
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
