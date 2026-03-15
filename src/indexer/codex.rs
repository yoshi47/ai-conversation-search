use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use rusqlite::Connection;

use crate::db;
use crate::error::Result;
use crate::git_utils::resolve_repo_root;

const DEFAULT_CODEX_SESSIONS: &str = "~/.codex/sessions";
const CX_PREFIX: &str = "codex:";

pub struct CodexIndexer {
    search_db_path: String,
    sessions_dir: PathBuf,
    quiet: bool,
    repo_root_cache: HashMap<String, Option<String>>,
    uuid_re: Regex,
}

impl CodexIndexer {
    pub fn new(
        search_db_path: Option<&str>,
        sessions_dir: Option<&str>,
        quiet: bool,
    ) -> Self {
        Self {
            search_db_path: search_db_path
                .unwrap_or(db::DEFAULT_DB_PATH)
                .to_string(),
            sessions_dir: db::expand_path(sessions_dir.unwrap_or(DEFAULT_CODEX_SESSIONS)),
            quiet,
            repo_root_cache: HashMap::new(),
            uuid_re: Regex::new(
                r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})",
            )
            .unwrap(),
        }
    }

    fn log(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{}", msg);
        }
    }

    fn resolve_repo_root_cached(&mut self, worktree: &str) -> Option<String> {
        if let Some(cached) = self.repo_root_cache.get(worktree) {
            return cached.clone();
        }
        let result = resolve_repo_root(worktree);
        self.repo_root_cache
            .insert(worktree.to_string(), result.clone());
        result
    }

    fn find_session_files(&self, days_back: Option<i64>) -> Vec<PathBuf> {
        if !self.sessions_dir.exists() {
            return vec![];
        }

        if let Some(d) = days_back {
            let mut files = Vec::new();
            let today = chrono::Local::now().date_naive();
            for i in 0..=d {
                let date = today - chrono::TimeDelta::days(i);
                let day_dir = self.sessions_dir.join(date.format("%Y/%m/%d").to_string());
                if day_dir.exists() {
                    if let Ok(entries) = std::fs::read_dir(&day_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().map_or(false, |e| e == "jsonl") {
                                files.push(path);
                            }
                        }
                    }
                }
            }
            files.sort();
            files
        } else {
            let mut files = Vec::new();
            for entry in walkdir::WalkDir::new(&self.sessions_dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if entry.path().extension().map_or(false, |e| e == "jsonl") {
                    files.push(entry.path().to_path_buf());
                }
            }
            files.sort();
            files
        }
    }

    fn extract_uuid_from_filename(&self, filename: &str) -> String {
        if let Some(m) = self.uuid_re.find(filename) {
            m.as_str().to_string()
        } else {
            Path::new(filename)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        }
    }

    pub fn scan_and_index(&mut self, days_back: Option<i64>) -> Result<i32> {
        let session_files = self.find_session_files(days_back);
        if session_files.is_empty() {
            self.log("No Codex session files found");
            return Ok(0);
        }

        let conn = db::connect(&self.search_db_path, false)?;

        // Ensure sync table
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS codex_sync_state (
                file_path TEXT PRIMARY KEY,
                mtime REAL NOT NULL,
                indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
            )",
        )?;

        self.do_index(&conn, &session_files)
    }

    fn do_index(&mut self, conn: &Connection, session_files: &[PathBuf]) -> Result<i32> {
        let mut indexed_count = 0;

        conn.execute_batch("BEGIN;")?;

        for session_file in session_files {
            let mtime = match session_file.metadata() {
                Ok(m) => m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
                Err(_) => continue,
            };

            // Check if already indexed with same mtime
            if let Ok(existing_mtime) = conn.query_row(
                "SELECT mtime FROM codex_sync_state WHERE file_path = ?",
                [session_file.to_string_lossy().as_ref()],
                |row| row.get::<_, f64>(0),
            ) {
                if (existing_mtime - mtime).abs() < 0.001 {
                    continue;
                }
            }

            match self.index_session_file(conn, session_file) {
                Ok(count) if count > 0 => {
                    indexed_count += 1;
                }
                Ok(_) => {}
                Err(e) => {
                    self.log(&format!(
                        "  Error indexing {}: {}",
                        session_file
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        e
                    ));
                    continue;
                }
            }

            // Update sync state
            conn.execute(
                "INSERT OR REPLACE INTO codex_sync_state (file_path, mtime) VALUES (?, ?)",
                rusqlite::params![session_file.to_string_lossy().as_ref(), mtime],
            )?;
        }

        conn.execute_batch("COMMIT;")?;

        if indexed_count > 0 {
            self.log(&format!("Indexed {} Codex sessions", indexed_count));
        } else {
            self.log("No new Codex sessions to index");
        }

        Ok(indexed_count)
    }

    fn index_session_file(&mut self, conn: &Connection, session_file: &Path) -> Result<i32> {
        let content = std::fs::read_to_string(session_file)?;
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Ok(0);
        }

        // Parse first line for session metadata
        let first: serde_json::Value = serde_json::from_str(lines[0])?;
        if first.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            return Ok(0);
        }

        let payload = first.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        let session_uuid = payload
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                self.extract_uuid_from_filename(
                    &session_file.file_name().unwrap().to_string_lossy(),
                )
            });

        let session_id = format!("{}{}", CX_PREFIX, session_uuid);
        let cwd = payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let repo_root = if !cwd.is_empty() {
            self.resolve_repo_root_cached(&cwd)
        } else {
            None
        };

        // Delete existing data for re-index
        conn.execute("DELETE FROM messages WHERE session_id = ?", [&session_id])?;

        // Parse events
        let mut messages: Vec<(String, String, String)> = Vec::new(); // (role, timestamp, text)
        let mut title_parts: Vec<String> = Vec::new();

        for line in &lines[1..] {
            let event: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let event_payload = event.get("payload").cloned().unwrap_or(serde_json::Value::Null);
            let timestamp = event
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();

            match event_type {
                "event_msg" => {
                    let msg_type = event_payload.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match msg_type {
                        "user_message" => {
                            let text = event_payload
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("");
                            if !text.trim().is_empty() {
                                messages.push(("user".to_string(), timestamp, text.to_string()));
                                if title_parts.is_empty() {
                                    title_parts.push(text.chars().take(100).collect());
                                }
                            }
                        }
                        "agent_message" => {
                            let text = event_payload
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("");
                            if !text.trim().is_empty() {
                                messages.push(("assistant".to_string(), timestamp, text.to_string()));
                            }
                        }
                        "agent_reasoning" => {
                            let text = event_payload
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("");
                            if !text.trim().is_empty() {
                                messages.push((
                                    "assistant".to_string(),
                                    timestamp,
                                    format!("[Reasoning] {}", text),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                "response_item" => {
                    let item_type = event_payload.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match item_type {
                        "function_call" => {
                            let name = event_payload
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown");
                            let mut tool_text = format!("[Tool: {}]", name);

                            if let Some(args_str) = event_payload.get("arguments").and_then(|a| a.as_str()) {
                                if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) {
                                    if let Some(cmd) = args.get("command") {
                                        let cmd_str = if let Some(s) = cmd.as_str() {
                                            s.to_string()
                                        } else if let Some(arr) = cmd.as_array() {
                                            arr.iter()
                                                .filter_map(|v| v.as_str())
                                                .collect::<Vec<_>>()
                                                .join(" ")
                                        } else {
                                            String::new()
                                        };
                                        if !cmd_str.is_empty() {
                                            tool_text.push('\n');
                                            tool_text.push_str(&cmd_str);
                                        }
                                    }
                                }
                            }
                            messages.push(("assistant".to_string(), timestamp, tool_text));
                        }
                        "function_call_output" => {
                            let output_str = event_payload
                                .get("output")
                                .and_then(|o| o.as_str())
                                .unwrap_or("");
                            let output_text = if let Ok(output_data) =
                                serde_json::from_str::<serde_json::Value>(output_str)
                            {
                                if let Some(obj) = output_data.as_object() {
                                    obj.get("output")
                                        .and_then(|o| o.as_str())
                                        .map(|s| s.chars().take(500).collect::<String>())
                                        .unwrap_or_else(|| {
                                            output_str.chars().take(500).collect()
                                        })
                                } else {
                                    output_str.chars().take(500).collect()
                                }
                            } else {
                                output_str.chars().take(500).collect()
                            };

                            if !output_text.trim().is_empty() {
                                messages.push((
                                    "assistant".to_string(),
                                    timestamp,
                                    format!("[Tool Output]\n{}", output_text),
                                ));
                            }
                        }
                        "message" => {
                            let role = event_payload.get("role").and_then(|r| r.as_str()).unwrap_or("");
                            if role == "user" || role == "developer" {
                                continue;
                            }
                            if let Some(content_parts) = event_payload.get("content").and_then(|c| c.as_array()) {
                                for part in content_parts {
                                    if part.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                            if !text.trim().is_empty() {
                                                messages.push(("assistant".to_string(), timestamp.clone(), text.to_string()));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        if messages.is_empty() {
            return Ok(0);
        }

        // Consolidate consecutive same-role messages
        let mut consolidated: Vec<(String, String, String)> = Vec::new();
        for (role, ts, text) in messages {
            if let Some(last) = consolidated.last_mut() {
                if last.0 == role {
                    last.2.push('\n');
                    last.2.push_str(&text);
                    continue;
                }
            }
            consolidated.push((role, ts, text));
        }

        // Insert messages
        let mut msg_count: i32 = 0;
        let mut first_timestamp: Option<String> = None;
        let mut last_timestamp: Option<String> = None;

        for (i, (role, ts, content)) in consolidated.iter().enumerate() {
            if content.trim().is_empty() {
                continue;
            }

            let message_uuid = format!("{}{}:{}", CX_PREFIX, session_uuid, i);

            if first_timestamp.is_none() {
                first_timestamp = Some(ts.clone());
            }
            last_timestamp = Some(ts.clone());

            conn.execute(
                "INSERT OR REPLACE INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    message_uuid,
                    session_id,
                    Option::<String>::None,
                    false,
                    i as i32,
                    ts,
                    role,
                    cwd,
                    session_file.to_string_lossy().as_ref(),
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

        let title = title_parts.first().map(|s| s.as_str()).unwrap_or("Untitled");
        let session_timestamp = payload
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("");

        conn.execute(
            "INSERT OR REPLACE INTO conversations (session_id, project_path, repo_root, conversation_file, root_message_uuid, leaf_message_uuid, conversation_summary, first_message_at, last_message_at, message_count, source) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'codex')",
            rusqlite::params![
                session_id,
                cwd,
                repo_root,
                session_file.to_string_lossy().as_ref(),
                format!("{}{}:0", CX_PREFIX, session_uuid),
                format!("{}{}:{}", CX_PREFIX, session_uuid, msg_count - 1),
                title,
                first_timestamp.as_deref().unwrap_or(session_timestamp),
                last_timestamp.as_deref().unwrap_or(session_timestamp),
                msg_count,
            ],
        )?;

        self.log(&format!(
            "  Indexed session: {} ({} messages)",
            &title[..std::cmp::min(title.len(), 60)],
            msg_count
        ));
        Ok(msg_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(include_str!("../../data/schema.sql")).unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    fn create_indexer(sessions_dir: &Path) -> CodexIndexer {
        CodexIndexer {
            search_db_path: String::new(),
            sessions_dir: sessions_dir.to_path_buf(),
            quiet: true,
            repo_root_cache: HashMap::new(),
            uuid_re: Regex::new(
                r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})",
            )
            .unwrap(),
        }
    }

    fn write_session_file(dir: &Path, filename: &str, lines: &[&str]) -> PathBuf {
        let date_dir = dir.join("sessions/2025/01/15");
        std::fs::create_dir_all(&date_dir).unwrap();
        let file_path = date_dir.join(filename);
        let mut f = std::fs::File::create(&file_path).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        file_path
    }

    #[test]
    fn test_basic_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "test-session.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Hello world"},"timestamp":"2025-01-15T10:00:01Z"}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Hi there!"},"timestamp":"2025-01-15T10:00:02Z"}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 2);

        let msg_count: i32 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(msg_count, 2);

        let first_content: String = conn
            .query_row(
                "SELECT full_content FROM messages ORDER BY depth ASC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_content, "Hello world");
    }

    #[test]
    fn test_fts_search_after_index() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "fts-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"11111111-2222-3333-4444-555555555555","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"How do I implement quicksort in Rust?"},"timestamp":"2025-01-15T10:00:01Z"}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Here is a quicksort implementation"},"timestamp":"2025-01-15T10:00:02Z"}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        indexer.index_session_file(&conn, &session_file).unwrap();

        let fts_count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_content_fts WHERE message_content_fts MATCH 'quicksort'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_count, 2);

        let no_match: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_content_fts WHERE message_content_fts MATCH 'nonexistentword'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(no_match, 0);
    }

    #[test]
    fn test_incremental_sync() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS codex_sync_state (
                file_path TEXT PRIMARY KEY,
                mtime REAL NOT NULL,
                indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .unwrap();

        let session_file = write_session_file(
            dir.path(),
            "inc-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"aaaaaaaa-1111-2222-3333-444444444444","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Test message"},"timestamp":"2025-01-15T10:00:01Z"}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        let files = vec![session_file.clone()];

        // First index
        let count1 = indexer.do_index(&conn, &files).unwrap();
        assert_eq!(count1, 1);

        // Second index without changes -> should skip (0 new sessions)
        let count2 = indexer.do_index(&conn, &files).unwrap();
        assert_eq!(count2, 0);

        // Reset sync state mtime to force re-indexing
        conn.execute(
            "UPDATE codex_sync_state SET mtime = 0 WHERE file_path = ?",
            [session_file.to_string_lossy().as_ref()],
        )
        .unwrap();

        let count3 = indexer.do_index(&conn, &files).unwrap();
        assert_eq!(count3, 1);
    }

    #[test]
    fn test_session_id_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "prefix-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"abcdefab-1234-5678-9abc-def012345678","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Test"},"timestamp":"2025-01-15T10:00:01Z"}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        indexer.index_session_file(&conn, &session_file).unwrap();

        let session_id: String = conn
            .query_row("SELECT session_id FROM messages LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            session_id.starts_with("codex:"),
            "session_id '{}' should start with 'codex:'",
            session_id
        );
    }

    #[test]
    fn test_tool_call_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "tool-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"aabbccdd-1122-3344-5566-778899aabbcc","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"List files"},"timestamp":"2025-01-15T10:00:01Z"}"#,
                r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":\"ls -la\"}"},"timestamp":"2025-01-15T10:00:02Z"}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 2);

        let tool_content: String = conn
            .query_row(
                "SELECT full_content FROM messages WHERE message_type = 'assistant'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(tool_content.contains("[Tool: shell]"), "tool content should contain tool name");
        assert!(tool_content.contains("ls -la"), "tool content should contain command");
    }

    #[test]
    fn test_empty_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "empty-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"00000000-0000-0000-0000-000000000000","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_corrupt_file_handling() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "corrupt-test.jsonl",
            &[
                r#"{"type":"not_session_meta","payload":{}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"This should not be indexed"},"timestamp":"2025-01-15T10:00:01Z"}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_extract_uuid_from_filename() {
        let indexer = create_indexer(Path::new("/nonexistent"));

        // Standard UUID in filename
        let uuid = indexer.extract_uuid_from_filename("session-abcdef01-2345-6789-abcd-ef0123456789.jsonl");
        assert_eq!(uuid, "abcdef01-2345-6789-abcd-ef0123456789");

        // No UUID -> fallback to file stem
        let fallback = indexer.extract_uuid_from_filename("some-file.jsonl");
        assert_eq!(fallback, "some-file");

        // UUID only
        let bare = indexer.extract_uuid_from_filename("12345678-1234-1234-1234-123456789abc.jsonl");
        assert_eq!(bare, "12345678-1234-1234-1234-123456789abc");
    }

    #[test]
    fn test_consolidation() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "consolidate-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"cccccccc-dddd-eeee-ffff-000000000000","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"First user msg"},"timestamp":"2025-01-15T10:00:01Z"}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Second user msg"},"timestamp":"2025-01-15T10:00:02Z"}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Agent reply"},"timestamp":"2025-01-15T10:00:03Z"}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        // Two consecutive user messages consolidated into one, plus one agent message = 2
        assert_eq!(count, 2);

        let user_content: String = conn
            .query_row(
                "SELECT full_content FROM messages WHERE message_type = 'user'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            user_content.contains("First user msg"),
            "consolidated content should contain first message"
        );
        assert!(
            user_content.contains("Second user msg"),
            "consolidated content should contain second message"
        );
    }

    #[test]
    fn test_conversation_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "meta-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"eeeeeeee-ffff-0000-1111-222222222222","cwd":"/tmp/myproject","timestamp":"2025-01-15T10:00:00Z"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Build my project"},"timestamp":"2025-01-15T10:00:01Z"}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Sure, building now"},"timestamp":"2025-01-15T10:00:02Z"}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Done building"},"timestamp":"2025-01-15T10:00:03Z"}"#,
            ],
        );

        let mut indexer = create_indexer(dir.path());
        indexer.index_session_file(&conn, &session_file).unwrap();

        let (summary, source, first_at, last_at, msg_count, project): (
            String,
            String,
            String,
            String,
            i32,
            String,
        ) = conn
            .query_row(
                "SELECT conversation_summary, source, first_message_at, last_message_at, message_count, project_path FROM conversations LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(summary, "Build my project");
        assert_eq!(source, "codex");
        assert_eq!(first_at, "2025-01-15T10:00:01Z");
        // Two agent messages are consolidated; the timestamp of the consolidated
        // message is from the first one in the run.
        assert_eq!(last_at, "2025-01-15T10:00:02Z");
        // user_message + two agent_messages (consolidated) = 2
        assert_eq!(msg_count, 2);
        assert_eq!(project, "/tmp/myproject");
    }
}
