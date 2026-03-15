use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::Connection;

use crate::db;
use crate::error::Result;
use crate::git_utils::resolve_repo_root;

const DEFAULT_OPENCODE_DB: &str = "~/.local/share/opencode/opencode.db";
const OC_PREFIX: &str = "oc:";

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
    repo_root_cache: HashMap<String, Option<String>>,
}

impl OpenCodeIndexer {
    pub fn new(
        search_db_path: Option<&str>,
        opencode_db_path: Option<&str>,
        quiet: bool,
    ) -> Self {
        let oc_path = opencode_db_path
            .map(|s| s.to_string())
            .unwrap_or_else(get_opencode_db_path);

        Self {
            search_db_path: search_db_path
                .unwrap_or(db::DEFAULT_DB_PATH)
                .to_string(),
            opencode_db_path: db::expand_path(&oc_path),
            quiet,
            repo_root_cache: HashMap::new(),
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

    fn resolve_repo_root_cached(&mut self, worktree: &str) -> Option<String> {
        if let Some(cached) = self.repo_root_cache.get(worktree) {
            return cached.clone();
        }
        let result = resolve_repo_root(worktree);
        self.repo_root_cache.insert(worktree.to_string(), result.clone());
        result
    }

    fn epoch_ms_to_iso(epoch_ms: i64) -> String {
        use chrono::{DateTime, Utc};
        let dt = DateTime::from_timestamp_millis(epoch_ms).unwrap_or(DateTime::<Utc>::MIN_UTC);
        dt.format("%Y-%m-%dT%H:%M:%S.%3fZ").to_string()
    }

    fn build_message_content(parts: &[(String,)]) -> String {
        let mut text_parts = Vec::new();
        for (data_str,) in parts {
            let data: serde_json::Value = match serde_json::from_str(data_str) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match data.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = data.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text_parts.push(text.to_string());
                        }
                    }
                }
                Some("tool") => {
                    let tool_name = data.get("tool").and_then(|t| t.as_str()).unwrap_or("unknown");
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
                Some("patch") | Some("file") => {
                    text_parts.push("[File change]".to_string());
                }
                _ => {}
            }
        }
        text_parts.join("\n")
    }

    pub fn scan_and_index(&mut self, days_back: Option<i64>) -> Result<i32> {
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

        let result = self.do_index(&oc_conn, &search_conn, days_back);
        result
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
        &mut self,
        oc_conn: &Connection,
        search_conn: &Connection,
        days_back: Option<i64>,
    ) -> Result<i32> {
        search_conn.execute_batch("BEGIN;")?;

        let last_sync = Self::get_last_sync_time(search_conn);

        let cutoff_ms: i64 = if let Some(d) = days_back {
            let cutoff = chrono::Local::now() - chrono::TimeDelta::days(d);
            cutoff.timestamp_millis()
        } else if let Some(ls) = last_sync {
            ls
        } else {
            0
        };

        let mut stmt = oc_conn.prepare(
            "SELECT s.id, s.project_id, s.title, s.directory,
                    s.time_created, s.time_updated,
                    p.worktree
             FROM session s
             JOIN project p ON s.project_id = p.id
             WHERE s.time_updated > ?
             ORDER BY s.time_updated DESC",
        )?;

        let sessions: Vec<(String, String, Option<String>, Option<String>, i64, i64, Option<String>)> =
            stmt.query_map([cutoff_ms], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if sessions.is_empty() {
            self.log("No new OpenCode sessions to index");
            return Ok(0);
        }

        self.log(&format!("Found {} OpenCode sessions to index", sessions.len()));

        let mut max_time_updated = cutoff_ms;
        let mut indexed_count = 0;

        for (id, _project_id, title, directory, time_created, time_updated, worktree) in &sessions {
            match self.index_session(oc_conn, search_conn, id, title.as_deref(), directory.as_deref(), *time_created, *time_updated, worktree.as_deref()) {
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

    #[allow(clippy::too_many_arguments)]
    fn index_session(
        &mut self,
        oc_conn: &Connection,
        search_conn: &Connection,
        session_id_raw: &str,
        title: Option<&str>,
        directory: Option<&str>,
        time_created: i64,
        time_updated: i64,
        worktree: Option<&str>,
    ) -> Result<i32> {
        let session_id = format!("{}{}", OC_PREFIX, session_id_raw);
        let work_dir = worktree.or(directory).unwrap_or("");

        let session_updated_iso = Self::epoch_ms_to_iso(time_updated);

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

        // Fetch messages
        let mut msg_stmt = oc_conn.prepare(
            "SELECT id, session_id, time_created, time_updated, data
             FROM message WHERE session_id = ? ORDER BY time_created ASC",
        )?;
        let messages: Vec<(String, String, i64, i64, String)> = msg_stmt
            .query_map([session_id_raw], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if messages.is_empty() {
            return Ok(0);
        }

        // Fetch all parts
        let mut parts_stmt = oc_conn.prepare(
            "SELECT id, message_id, data, time_created FROM part WHERE session_id = ? ORDER BY time_created ASC",
        )?;
        let mut parts_by_message: HashMap<String, Vec<(String,)>> = HashMap::new();
        let parts: Vec<(String, String, String, i64)> = parts_stmt
            .query_map([session_id_raw], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        for (_id, msg_id, data, _time) in &parts {
            parts_by_message
                .entry(msg_id.clone())
                .or_default()
                .push((data.clone(),));
        }

        let repo_root = if !work_dir.is_empty() {
            self.resolve_repo_root_cached(work_dir)
        } else {
            None
        };

        let mut msg_count: i32 = 0;
        let mut first_timestamp: Option<String> = None;

        for (msg_id, _sess_id, msg_time_created, _msg_time_updated, data_str) in &messages {
            let msg_data: serde_json::Value = match serde_json::from_str(data_str) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let role = match msg_data.get("role").and_then(|r| r.as_str()) {
                Some("user") | Some("assistant") => {
                    msg_data.get("role").unwrap().as_str().unwrap().to_string()
                }
                _ => continue,
            };

            let msg_parts = parts_by_message.get(msg_id).cloned().unwrap_or_default();
            let content = Self::build_message_content(&msg_parts);

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
                    msg_count,
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

        let display_title = title.unwrap_or("Untitled");
        let session_created_iso = Self::epoch_ms_to_iso(time_created);

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
                msg_count,
            ],
        )?;

        self.log(&format!(
            "  Indexed session: {} ({} messages)",
            display_title, msg_count
        ));
        Ok(msg_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let parts = vec![(r#"{"type":"text","text":"Hello world"}"#.to_string(),)];
        let result = OpenCodeIndexer::build_message_content(&parts);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_build_message_content_tool() {
        let parts = vec![(
            r#"{"type":"tool","tool":"bash","state":{"input":{"command":"ls"}}}"#.to_string(),
        )];
        let result = OpenCodeIndexer::build_message_content(&parts);
        assert_eq!(result, "[Tool: bash]\nls");
    }

    #[test]
    fn test_build_message_content_file_change() {
        let parts = vec![(r#"{"type":"patch","path":"src/main.rs"}"#.to_string(),)];
        let result = OpenCodeIndexer::build_message_content(&parts);
        assert_eq!(result, "[File change]");

        let parts_file = vec![(r#"{"type":"file","path":"README.md"}"#.to_string(),)];
        let result_file = OpenCodeIndexer::build_message_content(&parts_file);
        assert_eq!(result_file, "[File change]");
    }

    #[test]
    fn test_build_message_content_empty() {
        let parts: Vec<(String,)> = vec![];
        let result = OpenCodeIndexer::build_message_content(&parts);
        assert_eq!(result, "");
    }

    #[test]
    fn test_build_message_content_mixed() {
        let parts = vec![
            (r#"{"type":"text","text":"First line"}"#.to_string(),),
            (
                r#"{"type":"tool","tool":"grep","state":{"input":{"command":"grep -r foo"}}}"#
                    .to_string(),
            ),
            (r#"{"type":"patch","path":"lib.rs"}"#.to_string(),),
            (r#"{"type":"text","text":"Summary"}"#.to_string(),),
        ];
        let result = OpenCodeIndexer::build_message_content(&parts);
        assert_eq!(
            result,
            "First line\n[Tool: grep]\ngrep -r foo\n[File change]\nSummary"
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
