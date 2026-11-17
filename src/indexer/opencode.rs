use std::path::PathBuf;

use rusqlite::Connection;

use crate::db;
use crate::error::Result;

const DEFAULT_OPENCODE_DB: &str = "~/.local/share/opencode/opencode.db";
const OC_PREFIX: &str = "oc:";

struct SessionInfo<'a> {
    session_id_raw: &'a str,
    title: Option<&'a str>,
    directory: Option<&'a str>,
    time_created: i64,
    time_updated: i64,
    worktree: Option<&'a str>,
}

pub fn get_opencode_db_path() -> String {
    if let Ok(home) = std::env::var("OPENCODE_HOME") {
        format!("{}/opencode.db", home)
    } else {
        DEFAULT_OPENCODE_DB.to_string()
    }
}

pub struct OpenCodeIndexer {
    search_db_path: String,
    opencode_db_path: PathBuf,
    quiet: bool,
}

impl OpenCodeIndexer {
    pub fn new(search_db_path: Option<&str>, opencode_db_path: Option<&str>, quiet: bool) -> Self {
        let oc_path = opencode_db_path
            .map(|s| s.to_string())
            .unwrap_or_else(get_opencode_db_path);

        Self {
            search_db_path: search_db_path.unwrap_or(db::DEFAULT_DB_PATH).to_string(),
            opencode_db_path: db::expand_path(&oc_path),
            quiet,
        }
    }

    fn log(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{}", msg);
        }
    }

    fn connect_opencode(&self) -> Result<Option<Connection>> {
        if !self.opencode_db_path.exists() {
            self.log(&format!(
                "OpenCode DB not found: {}",
                self.opencode_db_path.display()
            ));
            return Ok(None);
        }

        // Not a `file:...?mode=ro` URI: the path is not percent-encoded, so `?`, `#`
        // or `%` in it would open a different file.
        let conn = Connection::open_with_flags(
            &self.opencode_db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Some(conn))
    }

    fn epoch_ms_to_iso(epoch_ms: i64) -> String {
        use chrono::{DateTime, Utc};
        let dt = DateTime::from_timestamp_millis(epoch_ms).unwrap_or(DateTime::<Utc>::MIN_UTC);
        dt.format("%Y-%m-%dT%H:%M:%S.%3fZ").to_string()
    }

    fn build_message_content(parts: &[serde_json::Value]) -> String {
        let mut text_parts = Vec::new();
        for data in parts {
            match data.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = data.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text_parts.push(text.to_string());
                        }
                    }
                }
                Some("tool") => {
                    let tool_name = data
                        .get("name")
                        .and_then(|t| t.as_str())
                        .unwrap_or("unknown");
                    text_parts.push(format!("[Tool: {}]", tool_name));
                    if let Some(state) = data.get("state").and_then(|s| s.as_object()) {
                        if let Some(input) = state.get("input").and_then(|i| i.as_object()) {
                            if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
                                if !cmd.is_empty() {
                                    text_parts.push(cmd.to_string());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        text_parts.join("\n")
    }

    pub fn scan_and_index(&self, days_back: Option<i64>) -> Result<usize> {
        let oc_conn = match self.connect_opencode()? {
            Some(c) => c,
            None => return Ok(0),
        };

        let search_conn = db::connect(&self.search_db_path, false)?;

        // Ensure sync table
        search_conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS opencode_sync_state (
                key TEXT PRIMARY KEY,
                value TEXT
            )",
        )?;

        self.do_index(&oc_conn, &search_conn, days_back)
    }

    fn get_last_sync_time(conn: &Connection) -> Option<i64> {
        conn.query_row(
            "SELECT value FROM opencode_sync_state WHERE key = 'last_sync_time'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok())
    }

    fn set_last_sync_time(conn: &Connection, time_ms: i64) {
        let _ = conn.execute(
            "INSERT OR REPLACE INTO opencode_sync_state (key, value) VALUES ('last_sync_time', ?)",
            [time_ms.to_string()],
        );
    }

    fn do_index(
        &self,
        oc_conn: &Connection,
        search_conn: &Connection,
        days_back: Option<i64>,
    ) -> Result<usize> {
        let last_sync = Self::get_last_sync_time(search_conn);

        let cutoff_ms: i64 = if let Some(d) = days_back {
            let cutoff = chrono::Local::now() - chrono::TimeDelta::days(d);
            cutoff.timestamp_millis()
        } else {
            last_sync.unwrap_or_default()
        };

        let (sessions, skipped) = fetch_sessions(oc_conn, cutoff_ms)?;
        if skipped > 0 {
            // Even under --quiet: an upstream schema change would otherwise just index nothing.
            eprintln!("  Skipped {} unreadable OpenCode session row(s)", skipped);
        }

        if sessions.is_empty() {
            self.log("No new OpenCode sessions to index");
            return Ok(0);
        }

        self.log(&format!(
            "Found {} OpenCode sessions to index",
            sessions.len()
        ));

        let mut max_time_updated = cutoff_ms;
        let mut min_failed_time_updated: Option<i64> = None;
        let mut indexed_count: usize = 0;

        for (id, title, directory, time_created, time_updated, worktree) in &sessions {
            let info = SessionInfo {
                session_id_raw: id,
                title: title.as_deref(),
                directory: directory.as_deref(),
                time_created: *time_created,
                time_updated: *time_updated,
                worktree: worktree.as_deref(),
            };
            // index_session deletes the old rows before inserting, so each session gets
            // its own transaction: a failure must roll back the delete, not commit it.
            let tx = search_conn.unchecked_transaction()?;
            let outcome = match self.index_session(oc_conn, &tx, &info) {
                Ok(count) => tx.commit().map(|()| count).map_err(Into::into),
                Err(e) => Err(e),
            };
            match outcome {
                Ok(count) => {
                    if count > 0 {
                        indexed_count += 1;
                    }
                    max_time_updated = max_time_updated.max(*time_updated);
                }
                Err(e) => {
                    // Even under --quiet: the background indexer's stderr is its only record.
                    eprintln!("  Error indexing session {}: {}", id, e);
                    min_failed_time_updated = Some(
                        min_failed_time_updated.map_or(*time_updated, |t| t.min(*time_updated)),
                    );
                }
            }
        }

        // Stop short of the oldest failed session so the next cursor-based run retries it.
        let cursor = match min_failed_time_updated {
            Some(failed) => max_time_updated.min(failed - 1),
            None => max_time_updated,
        };
        if cursor > cutoff_ms {
            Self::set_last_sync_time(search_conn, cursor);
        }

        self.log(&format!("Indexed {} OpenCode sessions", indexed_count));
        Ok(indexed_count)
    }

    fn index_session(
        &self,
        oc_conn: &Connection,
        search_conn: &Connection,
        info: &SessionInfo<'_>,
    ) -> Result<usize> {
        let session_id = format!("{}{}", OC_PREFIX, info.session_id_raw);
        let work_dir = info.worktree.or(info.directory).unwrap_or("");

        let session_updated_iso = Self::epoch_ms_to_iso(info.time_updated);

        // Check if already up to date
        if let Ok(existing_last) = search_conn.query_row(
            "SELECT last_message_at FROM conversations WHERE session_id = ?",
            [&session_id],
            |row| row.get::<_, String>(0),
        ) {
            if existing_last == session_updated_iso {
                // Up to date, but rows indexed before model tracking have NULL
                // models. Re-index those instead of skipping, so one pass
                // backfills them without a full database rebuild.
                let stored_model: Option<String> = search_conn
                    .query_row(
                        "SELECT model FROM conversations WHERE session_id = ?",
                        [&session_id],
                        |row| row.get(0),
                    )
                    .unwrap_or(None);
                if stored_model
                    .as_deref()
                    .is_some_and(|m| !m.trim().is_empty())
                {
                    return Ok(0);
                }
            }
            // Delete existing for re-index
            search_conn.execute("DELETE FROM messages WHERE session_id = ?", [&session_id])?;
        }

        let (messages, skipped) = fetch_messages(oc_conn, info.session_id_raw)?;
        if skipped > 0 {
            // Even under --quiet: an upstream schema change would otherwise just index nothing.
            eprintln!(
                "  Skipped {} unreadable message(s) in OpenCode session {}",
                skipped, info.session_id_raw
            );
        }

        let session_model = super::distinct_model_list(
            &messages
                .iter()
                .map(|(_, _, _, _, model)| model.clone())
                .collect::<Vec<_>>(),
        );

        let mut msg_count: usize = 0;
        let mut first_timestamp: Option<String> = None;
        let mut root_message_uuid: Option<String> = None;
        let mut leaf_message_uuid: Option<String> = None;

        for (msg_id, role, msg_time_created, content, model) in &messages {
            if content.trim().is_empty() {
                continue;
            }

            let timestamp = Self::epoch_ms_to_iso(*msg_time_created);
            let message_uuid = format!("{}{}", OC_PREFIX, msg_id);

            if first_timestamp.is_none() {
                first_timestamp = Some(timestamp.clone());
            }
            if root_message_uuid.is_none() {
                root_message_uuid = Some(message_uuid.clone());
            }
            leaf_message_uuid = Some(message_uuid.clone());
            search_conn.execute(
                "INSERT OR REPLACE INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise, model) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    message_uuid,
                    session_id,
                    Option::<String>::None,
                    false,
                    msg_count as i64,
                    timestamp,
                    role,
                    work_dir,
                    self.opencode_db_path.to_string_lossy(),
                    content,
                    false,
                    false,
                    model,
                ],
            )?;
            msg_count += 1;
        }

        if msg_count == 0 {
            // A re-index that leaves nothing searchable must not keep a conversations row
            // pointing at the messages deleted above.
            search_conn.execute(
                "DELETE FROM conversations WHERE session_id = ?",
                [&session_id],
            )?;
            return Ok(0);
        }

        let repo_root = if !work_dir.is_empty() {
            super::resolve_repo_root_cached(search_conn, work_dir)
        } else {
            None
        };

        // Blank-rejecting, not just None-rejecting: a whitespace-only upstream title would
        // otherwise be stored and render as an empty row in `list`.
        let display_title = info
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or("Untitled");
        let session_created_iso = Self::epoch_ms_to_iso(info.time_created);

        search_conn.execute(
            "INSERT OR REPLACE INTO conversations (session_id, project_path, repo_root, conversation_file, root_message_uuid, leaf_message_uuid, conversation_summary, first_message_at, last_message_at, message_count, source, model) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'opencode', ?)",
            rusqlite::params![
                session_id,
                work_dir,
                repo_root,
                self.opencode_db_path.to_string_lossy(),
                root_message_uuid,
                leaf_message_uuid,
                display_title,
                first_timestamp.as_deref().unwrap_or(&session_created_iso),
                session_updated_iso,
                msg_count as i64,
                session_model,
            ],
        )?;

        self.log(&format!(
            "  Indexed session: {} ({} messages)",
            display_title, msg_count
        ));
        Ok(msg_count)
    }
}

/// (id, title, directory, time_created, time_updated, worktree)
type SessionRow = (
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<String>,
);

/// Sessions updated after `cutoff_ms`, plus the number of rows that could not be read.
fn fetch_sessions(conn: &Connection, cutoff_ms: i64) -> Result<(Vec<SessionRow>, usize)> {
    // session_v2.time_updated is not bumped on every appended message, so using it
    // alone as the sync cursor would miss sessions that grew after the last run.
    let mut stmt = conn.prepare(
        "SELECT id, title, directory, time_created, time_updated, worktree FROM (
             SELECT s.id, s.title, s.directory, s.time_created,
                    MAX(s.time_updated, COALESCE(
                        (SELECT MAX(m.time_updated) FROM session_message m WHERE m.session_id = s.id),
                        0)) AS time_updated,
                    p.worktree
             FROM session_v2 s
             JOIN project p ON s.project_id = p.id)
         WHERE time_updated > ?
         ORDER BY time_updated DESC",
    )?;
    let mut rows = Vec::new();
    let mut skipped = 0;
    for row in stmt.query_map([cutoff_ms], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })? {
        match row {
            Ok(row) => rows.push(row),
            Err(_) => skipped += 1,
        }
    }
    Ok((rows, skipped))
}

/// (message id, role, time_created, content, model) for user/assistant messages, in order.
type MessageRow = (String, String, i64, String, Option<String>);

/// Extract `provider/id` from an OpenCode assistant message envelope.
///
/// Returns None for user messages (no `model` key) and for empty ids, so
/// "unknown" stays NULL rather than an empty string in the index.
fn extract_model(data: &serde_json::Value, role: &str) -> Option<String> {
    if role != "assistant" {
        return None;
    }
    let model = data.get("model")?;
    let id = model
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let provider = model
        .get("providerID")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match provider {
        Some(p) => Some(format!("{}/{}", p, id)),
        None => Some(id.to_string()),
    }
}

/// Messages of `session_id`, plus the number of rows or JSON envelopes that could not be read.
fn fetch_messages(conn: &Connection, session_id: &str) -> Result<(Vec<MessageRow>, usize)> {
    let mut stmt = conn.prepare(
        "SELECT id, type, time_created, data FROM session_message
         WHERE session_id = ? AND type IN ('user', 'assistant')
         ORDER BY seq ASC",
    )?;
    let mut rows = Vec::new();
    let mut skipped = 0;
    for row in stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })? {
        let Ok((id, role, time_created, data)) = row else {
            skipped += 1;
            continue;
        };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&data) else {
            skipped += 1;
            continue;
        };
        // Parts are embedded in the message: user text is `text`, assistant parts are `content[]`.
        let content = if role == "user" {
            data.get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string()
        } else {
            let parts = data
                .get("content")
                .and_then(|c| c.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            OpenCodeIndexer::build_message_content(parts)
        };
        let model = extract_model(&data, &role);
        rows.push((id, role, time_created, content, model));
    }
    Ok((rows, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(parts: &[&str]) -> Vec<serde_json::Value> {
        parts
            .iter()
            .map(|p| serde_json::from_str(p).unwrap())
            .collect()
    }

    #[test]
    fn test_epoch_ms_to_iso() {
        let result = OpenCodeIndexer::epoch_ms_to_iso(1705312800000);
        assert_eq!(result, "2024-01-15T10:00:00.000Z");
    }

    #[test]
    fn test_epoch_ms_to_iso_zero() {
        let result = OpenCodeIndexer::epoch_ms_to_iso(0);
        assert_eq!(result, "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn test_build_message_content_text() {
        let parts = parse(&[r#"{"type":"text","text":"Hello world"}"#]);
        let result = OpenCodeIndexer::build_message_content(&parts);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_build_message_content_tool() {
        let parts = parse(&[r#"{"type":"tool","name":"bash","state":{"input":{"command":"ls"}}}"#]);
        let result = OpenCodeIndexer::build_message_content(&parts);
        assert_eq!(result, "[Tool: bash]\nls");
    }

    #[test]
    fn test_build_message_content_empty() {
        let result = OpenCodeIndexer::build_message_content(&[]);
        assert_eq!(result, "");
    }

    #[test]
    fn test_build_message_content_mixed() {
        let parts = parse(&[
            r#"{"type":"text","text":"First line"}"#,
            r#"{"type":"tool","name":"grep","state":{"input":{"command":"grep -r foo"}}}"#,
            r#"{"type":"reasoning","text":"hidden"}"#,
            r#"{"type":"text","text":"Summary"}"#,
        ]);
        let result = OpenCodeIndexer::build_message_content(&parts);
        assert_eq!(result, "First line\n[Tool: grep]\ngrep -r foo\nSummary");
    }

    fn oc_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
            CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, title TEXT,
                directory TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
            CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL);
            INSERT INTO project VALUES ('p1', '/repo');
            INSERT INTO session_v2 VALUES ('s1', 'p1', 'T', '/repo/sub', 100, 200);
            INSERT INTO session_message VALUES
              ('m3', 's1', 'assistant', 3, 150, 900,
               '{"content":[{"type":"reasoning","text":"hidden"},{"type":"text","text":"answer"},{"type":"tool","name":"shell","state":{"input":{"command":"ls"}}}]}'),
              ('m1', 's1', 'user', 1, 110, 110, '{"text":"question","files":[]}'),
              ('m2', 's1', 'synthetic', 2, 120, 120, '{"text":"<system-reminder>"}'),
              ('m4', 's1', 'system', 4, 160, 160, '{"text":"date changed"}');
            "#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_fetch_messages_keeps_only_user_and_assistant_in_seq_order() {
        let (rows, skipped) = fetch_messages(&oc_db(), "s1").unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(
            rows,
            vec![
                ("m1".into(), "user".into(), 110, "question".into(), None),
                (
                    "m3".into(),
                    "assistant".into(),
                    150,
                    "answer\n[Tool: shell]\nls".into(),
                    None,
                ),
            ]
        );
    }

    #[test]
    fn test_extract_model_provider_and_id() {
        let data: serde_json::Value = serde_json::from_str(
            r#"{"model":{"id":"muse-spark-1.3-contributor-free","providerID":"opencode","variant":"default"},"content":[]}"#,
        )
        .unwrap();
        assert_eq!(
            extract_model(&data, "assistant").as_deref(),
            Some("opencode/muse-spark-1.3-contributor-free")
        );
        // User messages never carry a model, even if the envelope has one.
        assert_eq!(extract_model(&data, "user"), None);
        let bare: serde_json::Value =
            serde_json::from_str(r#"{"model":{"id":"gpt-5"},"content":[]}"#).unwrap();
        assert_eq!(extract_model(&bare, "assistant").as_deref(), Some("gpt-5"));
        let empty: serde_json::Value = serde_json::from_str(r#"{"content":[]}"#).unwrap();
        assert_eq!(extract_model(&empty, "assistant"), None);
    }

    #[test]
    fn test_fetch_sessions_uses_latest_message_time_as_updated() {
        // session_v2.time_updated (200) lags the last message (900); the cursor must use 900
        // or a session that grew after the last sync would be skipped.
        let conn = oc_db();
        let (rows, skipped) = fetch_sessions(&conn, 500).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(
            rows,
            vec![(
                "s1".into(),
                Some("T".into()),
                Some("/repo/sub".into()),
                100,
                900,
                Some("/repo".into())
            )]
        );
        assert!(fetch_sessions(&conn, 900).unwrap().0.is_empty());
    }

    fn search_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(include_str!("../../data/schema.sql"))
            .unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS opencode_sync_state (key TEXT PRIMARY KEY, value TEXT)",
        )
        .unwrap();
        conn
    }

    fn test_indexer() -> OpenCodeIndexer {
        OpenCodeIndexer {
            search_db_path: String::new(),
            opencode_db_path: PathBuf::from("/nonexistent/opencode.db"),
            quiet: true,
        }
    }

    fn contents(conn: &Connection, session_id: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT full_content FROM messages WHERE session_id = ? ORDER BY depth")
            .unwrap();
        stmt.query_map([session_id], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn fail_inserts_for(conn: &Connection, session_id: &str) {
        conn.execute_batch(&format!(
            "CREATE TRIGGER fail_insert BEFORE INSERT ON messages
             WHEN NEW.session_id = '{}' BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            session_id
        ))
        .unwrap();
    }

    #[test]
    fn test_failed_session_keeps_old_messages_and_holds_cursor() {
        let oc = oc_db();
        oc.execute_batch(
            r#"INSERT INTO session_v2 VALUES ('s2', 'p1', 'T2', '/repo', 100, 300);
               INSERT INTO session_message VALUES ('n1', 's2', 'user', 1, 110, 300, '{"text":"old s2"}');"#,
        )
        .unwrap();
        let search = search_db();
        let indexer = test_indexer();
        assert_eq!(indexer.do_index(&oc, &search, None).unwrap(), 2);

        // Both sessions grow; s2 (older) fails while s1 (newer) succeeds.
        oc.execute_batch(
            r#"INSERT INTO session_message VALUES ('n2', 's2', 'user', 2, 2000, 2000, '{"text":"new s2"}');
               INSERT INTO session_message VALUES ('m5', 's1', 'user', 5, 3000, 3000, '{"text":"new s1"}');"#,
        )
        .unwrap();
        fail_inserts_for(&search, "oc:s2");
        indexer.do_index(&oc, &search, None).unwrap();

        assert_eq!(contents(&search, "oc:s2"), vec!["old s2".to_string()]);
        assert!(contents(&search, "oc:s1").contains(&"new s1".to_string()));
        let cursor = OpenCodeIndexer::get_last_sync_time(&search).unwrap();
        assert!(
            cursor < 2000,
            "cursor {} skipped the failed session",
            cursor
        );

        search.execute_batch("DROP TRIGGER fail_insert;").unwrap();
        indexer.do_index(&oc, &search, None).unwrap();
        assert_eq!(
            contents(&search, "oc:s2"),
            vec!["old s2".to_string(), "new s2".to_string()]
        );
    }

    /// A DB damaged before the fix (messages deleted, conversations row left with the
    /// old last_message_at) is repaired by a full `index --all` pass.
    #[test]
    fn test_full_pass_repairs_session_whose_messages_were_deleted() {
        let oc = oc_db();
        // Inside the 9999-day window that `index --all` passes.
        oc.execute(
            "UPDATE session_message SET time_updated = ? WHERE id = 'm3'",
            [chrono::Utc::now().timestamp_millis()],
        )
        .unwrap();
        let search = search_db();
        let indexer = test_indexer();
        indexer.do_index(&oc, &search, None).unwrap();

        search
            .execute_batch(
                "DELETE FROM messages WHERE session_id = 'oc:s1';
                 UPDATE conversations SET last_message_at = '1970-01-01T00:00:00.500Z'
                 WHERE session_id = 'oc:s1';",
            )
            .unwrap();
        indexer.do_index(&oc, &search, Some(9999)).unwrap();
        assert_eq!(
            contents(&search, "oc:s1"),
            vec![
                "question".to_string(),
                "answer\n[Tool: shell]\nls".to_string()
            ]
        );
    }

    #[test]
    fn test_root_and_leaf_point_at_indexed_messages() {
        let oc = oc_db();
        oc.execute_batch(
            r#"INSERT INTO session_message VALUES
                 ('m0', 's1', 'user', 0, 105, 105, '{"text":"  "}'),
                 ('m9', 's1', 'assistant', 9, 170, 170, '{"content":[]}');"#,
        )
        .unwrap();
        let search = search_db();
        test_indexer().do_index(&oc, &search, None).unwrap();
        let (root, leaf): (String, String) = search
            .query_row(
                "SELECT root_message_uuid, leaf_message_uuid FROM conversations WHERE session_id = 'oc:s1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((root.as_str(), leaf.as_str()), ("oc:m1", "oc:m3"));
    }

    #[test]
    fn test_reindex_with_no_content_drops_conversation_row() {
        let oc = oc_db();
        let search = search_db();
        let indexer = test_indexer();
        indexer.do_index(&oc, &search, None).unwrap();

        oc.execute_batch(
            r#"UPDATE session_message SET data = '{"text":""}' WHERE id = 'm1';
               UPDATE session_message SET data = '{"content":[]}', time_updated = 1000 WHERE id = 'm3';"#,
        )
        .unwrap();
        indexer.do_index(&oc, &search, None).unwrap();
        let rows: i64 = search
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE session_id = 'oc:s1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn test_fetch_messages_reports_skipped_rows() {
        let oc = oc_db();
        oc.execute_batch(
            r#"INSERT INTO session_message VALUES
                 ('b1', 's1', 'user', 5, 170, 170, 'not json'),
                 ('b2', 's1', 'user', 6, 180, 180, X'00');"#,
        )
        .unwrap();
        let (rows, skipped) = fetch_messages(&oc, "s1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(skipped, 2);
    }

    #[test]
    fn test_fetch_sessions_reports_skipped_rows() {
        let oc = oc_db();
        oc.execute_batch("INSERT INTO session_v2 VALUES ('s2', 'p1', 'T2', '/repo', 'abc', 300);")
            .unwrap();
        let (rows, skipped) = fetch_sessions(&oc, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(skipped, 1);
    }

    fn write_oc_db(path: &std::path::Path) {
        let src = oc_db();
        src.execute("VACUUM INTO ?", [path.to_string_lossy()])
            .unwrap();
    }

    #[test]
    fn test_opencode_db_path_with_uri_metacharacters() {
        let dir = tempfile::tempdir().unwrap();
        let oc_dir = dir.path().join("a#b?c%41");
        std::fs::create_dir_all(&oc_dir).unwrap();
        let oc_path = oc_dir.join("opencode.db");
        write_oc_db(&oc_path);

        let search_path = dir.path().join("search.db");
        crate::schema::init_schema(&db::connect(search_path.to_str().unwrap(), false).unwrap())
            .unwrap();
        let indexer = OpenCodeIndexer::new(search_path.to_str(), oc_path.to_str(), true);
        assert_eq!(indexer.scan_and_index(None).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_unopenable_opencode_db_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let oc_path = dir.path().join("opencode.db");
        write_oc_db(&oc_path);
        std::fs::set_permissions(&oc_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let search_path = dir.path().join("search.db");
        let indexer = OpenCodeIndexer::new(search_path.to_str(), oc_path.to_str(), true);
        let result = indexer.scan_and_index(None);
        std::fs::set_permissions(&oc_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            result.is_err(),
            "expected an error, got {:?}",
            result.map(|_| ())
        );
    }

    #[test]
    fn test_get_opencode_db_path_default() {
        // Temporarily remove OPENCODE_HOME if set
        let orig = std::env::var("OPENCODE_HOME").ok();
        std::env::remove_var("OPENCODE_HOME");

        let path = get_opencode_db_path();
        assert_eq!(path, "~/.local/share/opencode/opencode.db");

        // Restore
        if let Some(val) = orig {
            std::env::set_var("OPENCODE_HOME", val);
        }
    }

    #[test]
    fn test_oc_prefix() {
        assert_eq!(OC_PREFIX, "oc:");
    }
}
