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

    fn connect_opencode(&self) -> Option<Connection> {
        if !self.opencode_db_path.exists() {
            self.log(&format!(
                "OpenCode DB not found: {}",
                self.opencode_db_path.display()
            ));
            return None;
        }

        let uri = format!("file:{}?mode=ro", self.opencode_db_path.display());
        Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()
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
        let oc_conn = match self.connect_opencode() {
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
        search_conn.execute_batch("BEGIN;")?;

        let last_sync = Self::get_last_sync_time(search_conn);

        let cutoff_ms: i64 = if let Some(d) = days_back {
            let cutoff = chrono::Local::now() - chrono::TimeDelta::days(d);
            cutoff.timestamp_millis()
        } else {
            last_sync.unwrap_or_default()
        };

        let sessions = fetch_sessions(oc_conn, cutoff_ms)?;

        if sessions.is_empty() {
            self.log("No new OpenCode sessions to index");
            return Ok(0);
        }

        self.log(&format!(
            "Found {} OpenCode sessions to index",
            sessions.len()
        ));

        let mut max_time_updated = cutoff_ms;
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
            match self.index_session(oc_conn, search_conn, &info) {
                Ok(count) if count > 0 => {
                    indexed_count += 1;
                    if *time_updated > max_time_updated {
                        max_time_updated = *time_updated;
                    }
                }
                Ok(_) => {
                    if *time_updated > max_time_updated {
                        max_time_updated = *time_updated;
                    }
                }
                Err(e) => {
                    self.log(&format!("  Error indexing session {}: {}", id, e));
                }
            }
        }

        search_conn.execute_batch("COMMIT;")?;

        if max_time_updated > cutoff_ms {
            Self::set_last_sync_time(search_conn, max_time_updated);
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
                return Ok(0);
            }
            // Delete existing for re-index
            search_conn.execute("DELETE FROM messages WHERE session_id = ?", [&session_id])?;
        }

        let messages = fetch_messages(oc_conn, info.session_id_raw)?;

        if messages.is_empty() {
            return Ok(0);
        }

        let repo_root = if !work_dir.is_empty() {
            super::resolve_repo_root_cached(search_conn, work_dir)
        } else {
            None
        };

        let mut msg_count: usize = 0;
        let mut first_timestamp: Option<String> = None;

        for (msg_id, role, msg_time_created, content) in &messages {
            if content.trim().is_empty() {
                continue;
            }

            let timestamp = Self::epoch_ms_to_iso(*msg_time_created);
            let message_uuid = format!("{}{}", OC_PREFIX, msg_id);

            if first_timestamp.is_none() {
                first_timestamp = Some(timestamp.clone());
            }
            search_conn.execute(
                "INSERT OR REPLACE INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                ],
            )?;
            msg_count += 1;
        }

        if msg_count == 0 {
            return Ok(0);
        }

        // Blank-rejecting, not just None-rejecting: a whitespace-only upstream title would
        // otherwise be stored and render as an empty row in `list`.
        let display_title = info
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or("Untitled");
        let session_created_iso = Self::epoch_ms_to_iso(info.time_created);

        search_conn.execute(
            "INSERT OR REPLACE INTO conversations (session_id, project_path, repo_root, conversation_file, root_message_uuid, leaf_message_uuid, conversation_summary, first_message_at, last_message_at, message_count, source) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'opencode')",
            rusqlite::params![
                session_id,
                work_dir,
                repo_root,
                self.opencode_db_path.to_string_lossy(),
                format!("{}{}", OC_PREFIX, messages[0].0),
                format!("{}{}", OC_PREFIX, messages.last().unwrap().0),
                display_title,
                first_timestamp.as_deref().unwrap_or(&session_created_iso),
                session_updated_iso,
                msg_count as i64,
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

fn fetch_sessions(conn: &Connection, cutoff_ms: i64) -> Result<Vec<SessionRow>> {
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
    let rows = stmt
        .query_map([cutoff_ms], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// (message id, role, time_created, content) for user/assistant messages, in order.
type MessageRow = (String, String, i64, String);

fn fetch_messages(conn: &Connection, session_id: &str) -> Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, type, time_created, data FROM session_message
         WHERE session_id = ? AND type IN ('user', 'assistant')
         ORDER BY seq ASC",
    )?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(id, role, time_created, data)| {
            let data: serde_json::Value = serde_json::from_str(&data).ok()?;
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
            Some((id, role, time_created, content))
        })
        .collect();
    Ok(rows)
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
        let rows = fetch_messages(&oc_db(), "s1").unwrap();
        assert_eq!(
            rows,
            vec![
                ("m1".into(), "user".into(), 110, "question".into()),
                (
                    "m3".into(),
                    "assistant".into(),
                    150,
                    "answer\n[Tool: shell]\nls".into()
                ),
            ]
        );
    }

    #[test]
    fn test_fetch_sessions_uses_latest_message_time_as_updated() {
        // session_v2.time_updated (200) lags the last message (900); the cursor must use 900
        // or a session that grew after the last sync would be skipped.
        let conn = oc_db();
        let rows = fetch_sessions(&conn, 500).unwrap();
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
        assert!(fetch_sessions(&conn, 900).unwrap().is_empty());
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
