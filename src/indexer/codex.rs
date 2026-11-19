use std::path::{Path, PathBuf};

use regex::Regex;
use rusqlite::Connection;

use crate::db;
use crate::error::{AppError, Result};

const DEFAULT_CODEX_SESSIONS: &str = "~/.codex/sessions";
const CX_PREFIX: &str = "codex:";
// Codex compresses rollouts idle for 7+ days to `<name>.jsonl.zst` and removes the
// plain file; resuming the session decompresses it back to `<name>.jsonl`.

// Compared as bytes, not `to_str()`: a non-UTF-8 file name must still be listed.
fn file_name_ends_with(path: &Path, suffix: &str) -> bool {
    path.file_name()
        .is_some_and(|n| n.as_encoded_bytes().ends_with(suffix.as_bytes()))
}

fn is_compressed_rollout(path: &Path) -> bool {
    file_name_ends_with(path, ".jsonl.zst")
}

fn is_rollout_file(path: &Path) -> bool {
    is_compressed_rollout(path) || file_name_ends_with(path, ".jsonl")
}

/// The `.jsonl` path a rollout has whether or not it is currently compressed. Sync state
/// is keyed by it: compression keeps both content and mtime, so keying by the physical
/// path would re-parse every session once Codex compresses it.
fn plain_rollout_path(path: &Path) -> PathBuf {
    if is_compressed_rollout(path) {
        // Strips only the trailing `.zst`; the name ends in `.jsonl.zst`.
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

fn read_rollout(path: &Path) -> Result<String> {
    if !is_compressed_rollout(path) {
        return Ok(std::fs::read_to_string(path)?);
    }
    let bytes = zstd::decode_all(std::fs::File::open(path)?)?;
    String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.utf8_error()).into())
}

/// Text blocks Codex injects with `role: "user"` in the protocol log. They are
/// harness context (AGENTS.md, sandbox policy, subagent notifications), not
/// something the human typed.
const INJECTED_USER_PREFIXES: &[&str] = &[
    "# AGENTS.md",
    "<environment_context>",
    "<user_instructions>",
    "<permissions instructions>",
    "<turn_aborted>",
    "<skills_instructions>",
    "<skill",
    "<user_action>",
    "<user_shell_command>",
    "<collaboration_mode>",
    "<multi_agent",
    "<subagent_notification>",
];

fn is_injected_user_text(text: &str) -> bool {
    let text = text.trim_start();
    INJECTED_USER_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Join the text of content parts whose `type` is one of `accepted_types`.
/// Codex uses `input_text` for user input and `output_text` for assistant
/// output in `response_item`, and plain `text` inside `item_completed`.
fn content_text(content: &serde_json::Value, accepted_types: &[&str]) -> String {
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    parts
        .iter()
        .filter(|part| {
            part.get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| accepted_types.contains(&t))
        })
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the human-written parts of a protocol-log user message, dropping
/// harness-injected blocks part by part so a message that mixes context and a
/// real prompt still keeps the prompt.
fn user_input_text(content: &serde_json::Value) -> String {
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    parts
        .iter()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("input_text"))
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
        .map(str::trim)
        .filter(|text| !text.is_empty() && !is_injected_user_text(text))
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct CodexIndexer {
    search_db_path: String,
    sessions_dir: PathBuf,
    quiet: bool,
    uuid_re: Regex,
}

impl CodexIndexer {
    pub fn new(search_db_path: Option<&str>, sessions_dir: Option<&str>, quiet: bool) -> Self {
        Self {
            search_db_path: search_db_path.unwrap_or(db::DEFAULT_DB_PATH).to_string(),
            sessions_dir: db::expand_path(sessions_dir.unwrap_or(DEFAULT_CODEX_SESSIONS)),
            quiet,
            uuid_re: Regex::new(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})")
                .unwrap(),
        }
    }

    fn log(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{}", msg);
        }
    }

    fn find_session_files(&self, days_back: Option<i64>) -> Vec<PathBuf> {
        if !self.sessions_dir.exists() {
            return vec![];
        }

        // Filter by mtime, not by the `YYYY/MM/DD` directory: that is the session's start
        // date, and resuming a session appends to the original rollout file.
        let cutoff = days_back.map(|d| {
            std::time::SystemTime::from(chrono::Local::now() - chrono::TimeDelta::days(d))
        });
        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(&self.sessions_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !is_rollout_file(entry.path()) {
                continue;
            }
            // Codex reads the plain file whenever both exist (mid-compression,
            // mid-decompression, or after an interrupted one), so we do too.
            if is_compressed_rollout(entry.path()) && plain_rollout_path(entry.path()).exists() {
                continue;
            }
            if let Some(cutoff) = cutoff {
                let Ok(mtime) = entry.path().metadata().and_then(|m| m.modified()) else {
                    continue;
                };
                if mtime < cutoff {
                    continue;
                }
            }
            files.push(entry.path().to_path_buf());
        }
        files.sort();
        files
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

    pub fn scan_and_index(&self, days_back: Option<i64>) -> Result<usize> {
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

    fn do_index(&self, conn: &Connection, session_files: &[PathBuf]) -> Result<usize> {
        let mut indexed_count: usize = 0;

        for session_file in session_files {
            // Recording a placeholder mtime (e.g. 0.0) would match on every later run and
            // skip the file forever, so leave it unrecorded and retry next time.
            let mtime = match session_file
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.duration_since(std::time::UNIX_EPOCH))
            {
                Ok(Ok(d)) => d.as_secs_f64(),
                // See the NotFound arm below.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Ok(Err(_)) | Err(_) => {
                    eprintln!(
                        "  Skipping {}: cannot read its modification time",
                        session_file.display()
                    );
                    continue;
                }
            };

            let sync_key = plain_rollout_path(session_file);
            // Check if already indexed with same mtime
            if let Ok(existing_mtime) = conn.query_row(
                "SELECT mtime FROM codex_sync_state WHERE file_path = ?",
                [sync_key.to_string_lossy().as_ref()],
                |row| row.get::<_, f64>(0),
            ) {
                if (existing_mtime - mtime).abs() < 0.001 {
                    continue;
                }
            }

            // index_session_file deletes the old rows before inserting, so each file gets its
            // own transaction: a failure must roll back the delete, not commit it.
            let tx = conn.unchecked_transaction()?;
            let outcome = self
                .index_session_file(&tx, session_file)
                .and_then(|count| {
                    tx.execute(
                        "INSERT OR REPLACE INTO codex_sync_state (file_path, mtime) VALUES (?, ?)",
                        rusqlite::params![sync_key.to_string_lossy().as_ref(), mtime],
                    )?;
                    tx.commit()?;
                    Ok(count)
                });
            match outcome {
                Ok(count) if count > 0 => {
                    indexed_count += 1;
                }
                Ok(_) => {}
                // Not an error: Codex swapped `.jsonl` and `.jsonl.zst` after we listed the
                // files, and the next run lists the other one. Logging it would put a false
                // failure in the background log every time a session is compressed or resumed.
                Err(AppError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                // Even under --quiet: the background indexer's stderr is its only record.
                Err(e) => {
                    eprintln!(
                        "  Error indexing {}: {}",
                        session_file
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        e
                    );
                }
            }
        }

        if indexed_count > 0 {
            self.log(&format!("Indexed {} Codex sessions", indexed_count));
        } else {
            self.log("No new Codex sessions to index");
        }

        Ok(indexed_count)
    }

    fn index_session_file(&self, conn: &Connection, session_file: &Path) -> Result<usize> {
        let content = read_rollout(session_file)?;
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Ok(0);
        }

        // Parse first line for session metadata
        let first: serde_json::Value = serde_json::from_str(lines[0])?;
        if first.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            return Ok(0);
        }

        let payload = first
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let session_uuid = payload
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                // The plain name, so the fallback id survives compression unchanged.
                self.extract_uuid_from_filename(
                    &plain_rollout_path(session_file)
                        .file_name()
                        .unwrap()
                        .to_string_lossy(),
                )
            });

        let session_id = format!("{}{}", CX_PREFIX, session_uuid);
        let cwd = payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let repo_root = if !cwd.is_empty() {
            super::resolve_repo_root_cached(conn, &cwd)
        } else {
            None
        };

        // Delete existing data for re-index
        conn.execute("DELETE FROM messages WHERE session_id = ?", [&session_id])?;

        // Parse events
        // (role, timestamp, text, model). Codex records the active model on
        // `turn_context` lines, so track the most recent one and attribute it
        // to subsequent assistant messages until the next turn starts.
        let mut messages: Vec<(String, String, String, Option<String>)> = Vec::new();
        let mut title_parts: Vec<String> = Vec::new();
        let mut current_model: Option<String> = None;

        // Paginated rollouts write the same turn twice: the protocol log
        // (`response_item/message`) and the display log
        // (`event_msg/item_completed`). Prefer the display log for user turns:
        // it contains only what the human typed, while the protocol copy also
        // carries injected AGENTS.md / environment context. Legacy rollouts
        // have `event_msg/user_message` instead, so the protocol copy is only a
        // fallback when neither event exists.
        let has_item_user = lines[1..].iter().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .is_some_and(|event| {
                    if event.get("type").and_then(|t| t.as_str()) != Some("event_msg")
                        || event
                            .get("payload")
                            .and_then(|p| p.get("type"))
                            .and_then(|t| t.as_str())
                            != Some("item_completed")
                    {
                        return false;
                    }
                    event
                        .get("payload")
                        .and_then(|p| p.get("item"))
                        .is_some_and(|item| {
                            item.get("type").and_then(|t| t.as_str()) == Some("UserMessage")
                                && item.get("content").is_some_and(|content| {
                                    !content_text(content, &["text"]).trim().is_empty()
                                })
                        })
                })
        });
        let has_legacy_user = lines[1..].iter().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .is_some_and(|event| {
                    event.get("type").and_then(|t| t.as_str()) == Some("event_msg")
                        && event
                            .get("payload")
                            .and_then(|p| p.get("type"))
                            .and_then(|t| t.as_str())
                            == Some("user_message")
                })
        });

        for line in &lines[1..] {
            let event: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let event_payload = event
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            // `turn_context` carries the model for the turns that follow it
            // (e.g. {"model": "gpt-5", ...}). Update the cursor and continue;
            // it is not itself a message.
            if event_type == "turn_context" {
                if let Some(model) = event_payload
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                {
                    current_model = Some(model.to_string());
                }
                continue;
            }
            let timestamp = event
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();

            match event_type {
                "event_msg" => {
                    let msg_type = event_payload
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    match msg_type {
                        "item_completed" => {
                            let item = event_payload.get("item");
                            if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                                != Some("UserMessage")
                            {
                                continue;
                            }
                            let text = item
                                .and_then(|i| i.get("content"))
                                .map(|content| content_text(content, &["text"]))
                                .unwrap_or_default();
                            if !text.trim().is_empty() {
                                if title_parts.is_empty() {
                                    title_parts.push(text.chars().take(100).collect());
                                }
                                messages.push(("user".to_string(), timestamp, text, None));
                            }
                        }
                        "user_message" => {
                            // Legacy history mode; paginated rollouts use
                            // item_completed/UserMessage above instead.
                            if has_item_user {
                                continue;
                            }
                            let text = event_payload
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("");
                            if !text.trim().is_empty() {
                                messages.push((
                                    "user".to_string(),
                                    timestamp,
                                    text.to_string(),
                                    None,
                                ));
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
                                messages.push((
                                    "assistant".to_string(),
                                    timestamp,
                                    text.to_string(),
                                    current_model.clone(),
                                ));
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
                                    current_model.clone(),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                "response_item" => {
                    let item_type = event_payload
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    match item_type {
                        "function_call" => {
                            let name = event_payload
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown");
                            let mut tool_text = format!("[Tool: {}]", name);

                            if let Some(args_str) =
                                event_payload.get("arguments").and_then(|a| a.as_str())
                            {
                                if let Ok(args) =
                                    serde_json::from_str::<serde_json::Value>(args_str)
                                {
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
                            messages.push((
                                "assistant".to_string(),
                                timestamp,
                                tool_text,
                                current_model.clone(),
                            ));
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
                                        .unwrap_or_else(|| output_str.chars().take(500).collect())
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
                                    current_model.clone(),
                                ));
                            }
                        }
                        "message" => {
                            let role = event_payload
                                .get("role")
                                .and_then(|r| r.as_str())
                                .unwrap_or("");
                            if role == "developer" {
                                continue;
                            }
                            if role == "user" {
                                // The clean copy is in event_msg/item_completed
                                // (or the legacy event_msg/user_message). Only
                                // fall back to the protocol log when neither
                                // event was written.
                                if has_item_user || has_legacy_user {
                                    continue;
                                }
                                let text = event_payload
                                    .get("content")
                                    .map(user_input_text)
                                    .unwrap_or_default();
                                if !text.trim().is_empty() {
                                    if title_parts.is_empty() {
                                        title_parts.push(text.chars().take(100).collect());
                                    }
                                    messages.push(("user".to_string(), timestamp, text, None));
                                }
                                continue;
                            }
                            let text = event_payload
                                .get("content")
                                .map(|content| content_text(content, &["output_text"]))
                                .unwrap_or_default();
                            if !text.trim().is_empty() {
                                messages.push((
                                    "assistant".to_string(),
                                    timestamp,
                                    text,
                                    current_model.clone(),
                                ));
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

        // Consolidate consecutive same-role messages. A model change breaks the
        // run: turns that switched models stay as separate rows so each keeps
        // its own attribution instead of merging into a "a,b" hybrid.
        let mut consolidated: Vec<(String, String, String, Option<String>)> = Vec::new();
        for (role, ts, text, model) in messages {
            if let Some(last) = consolidated.last_mut() {
                if last.0 == role && last.3 == model {
                    last.2.push('\n');
                    last.2.push_str(&text);
                    continue;
                }
            }
            consolidated.push((role, ts, text, model));
        }

        let session_model = super::distinct_model_list(
            &consolidated
                .iter()
                .map(|(_, _, _, model)| model.clone())
                .collect::<Vec<_>>(),
        );

        // Insert messages
        let mut msg_count: usize = 0;
        let mut first_timestamp: Option<String> = None;
        let mut last_timestamp: Option<String> = None;

        for (i, (role, ts, content, model)) in consolidated.iter().enumerate() {
            if content.trim().is_empty() {
                continue;
            }

            let message_uuid = format!("{}{}:{}", CX_PREFIX, session_uuid, i);

            if first_timestamp.is_none() {
                first_timestamp = Some(ts.clone());
            }
            last_timestamp = Some(ts.clone());

            conn.execute(
                "INSERT OR REPLACE INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise, model) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    message_uuid,
                    session_id,
                    Option::<String>::None,
                    false,
                    i as i64,
                    ts,
                    role,
                    cwd,
                    session_file.to_string_lossy().as_ref(),
                    content,
                    false,
                    false,
                    model,
                ],
            )?;
            msg_count += 1;
        }

        if msg_count == 0 {
            return Ok(0);
        }

        // Blank-rejecting, not just empty-vec-rejecting: a whitespace-only first part would
        // otherwise be stored and render as an empty row in `list`.
        let title = title_parts
            .first()
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Untitled");
        let session_timestamp = payload
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("");

        conn.execute(
            "INSERT OR REPLACE INTO conversations (session_id, project_path, repo_root, conversation_file, root_message_uuid, leaf_message_uuid, conversation_summary, first_message_at, last_message_at, message_count, source, model) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'codex', ?)",
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
                msg_count as i64,
                session_model,
            ],
        )?;

        // Truncate by chars, not bytes: titles are arbitrary user text, and a byte slice
        // panics whenever offset 60 lands inside a multi-byte character.
        self.log(&format!(
            "  Indexed session: {} ({} messages)",
            title.chars().take(60).collect::<String>(),
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
        conn.execute_batch(include_str!("../../data/schema.sql"))
            .unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    fn create_indexer(sessions_dir: &Path) -> CodexIndexer {
        CodexIndexer {
            search_db_path: String::new(),
            sessions_dir: sessions_dir.to_path_buf(),
            quiet: true,
            uuid_re: Regex::new(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})")
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

    fn message_rows(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT message_type, full_content FROM messages ORDER BY depth ASC")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
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

        let indexer = create_indexer(dir.path());
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
    fn test_turn_context_model_attribution() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "model-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"bbbbbbbb-cccc-dddd-eeee-ffffffffffff","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
                r#"{"type":"turn_context","payload":{"cwd":"/tmp","model":"gpt-5","effort":"high"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Hello"},"timestamp":"2025-01-15T10:00:01Z"}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Hi"},"timestamp":"2025-01-15T10:00:02Z"}"#,
                r#"{"type":"turn_context","payload":{"cwd":"/tmp","model":"gpt-5-codex","effort":"high"}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Follow-up"},"timestamp":"2025-01-15T10:00:03Z"}"#,
            ],
        );

        let indexer = create_indexer(dir.path());
        indexer.index_session_file(&conn, &session_file).unwrap();

        // user + 2 agent runs: the two agent messages use different models,
        // so they must not consolidate into one row.
        let models: Vec<Option<String>> = {
            let mut stmt = conn
                .prepare("SELECT model FROM messages ORDER BY depth ASC")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            models,
            vec![
                None,
                Some("gpt-5".to_string()),
                Some("gpt-5-codex".to_string())
            ]
        );
        let session_model: Option<String> = conn
            .query_row("SELECT model FROM conversations LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(session_model.as_deref(), Some("gpt-5,gpt-5-codex"));
    }

    /// The progress line truncates the session title by character count. `quiet` is
    /// turned off deliberately: that log is the only caller that truncates, so the panic
    /// is unreachable while quiet.
    #[test]
    fn test_non_ascii_title_does_not_panic_when_logging() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        // Leading ASCII shifts the 3-byte characters off a multiple of 3, so byte offset
        // 60 lands inside a character rather than neatly between two.
        let title = "DB: データベースのマイグレーションが失敗する原因を調べたい？続きを教えて";
        assert!(!title.is_char_boundary(60), "fixture no longer straddles");
        let session_file = write_session_file(
            dir.path(),
            "japanese-title.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"99999999-8888-7777-6666-555555555555","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#,
                &format!(
                    r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"{}"}},"timestamp":"2025-01-15T10:00:01Z"}}"#,
                    title
                ),
            ],
        );

        let mut indexer = create_indexer(dir.path());
        indexer.quiet = false;

        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 1);
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

        let indexer = create_indexer(dir.path());
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

        let indexer = create_indexer(dir.path());
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

    fn create_sync_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS codex_sync_state (
                file_path TEXT PRIMARY KEY,
                mtime REAL NOT NULL,
                indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .unwrap();
    }

    fn set_mtime(path: &Path, time: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn test_failed_file_keeps_old_messages_and_sync_state() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();
        create_sync_table(&conn);

        let meta = r#"{"type":"session_meta","payload":{"id":"56565656-7878-9090-1212-343434343434","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#;
        let user = r#"{"type":"event_msg","payload":{"type":"user_message","message":"old question"},"timestamp":"2025-01-15T10:00:01Z"}"#;
        let session_file = write_session_file(dir.path(), "atomic.jsonl", &[meta, user]);
        let indexer = create_indexer(dir.path());
        let files = vec![session_file.clone()];
        assert_eq!(indexer.do_index(&conn, &files).unwrap(), 1);
        let old_mtime: f64 = conn
            .query_row("SELECT mtime FROM codex_sync_state", [], |row| row.get(0))
            .unwrap();

        let agent = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"new answer"},"timestamp":"2025-01-15T10:00:02Z"}"#;
        write_session_file(dir.path(), "atomic.jsonl", &[meta, user, agent]);
        set_mtime(
            &session_file,
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000),
        );
        conn.execute_batch(
            "CREATE TRIGGER fail_insert BEFORE INSERT ON messages
             WHEN NEW.session_id = 'codex:56565656-7878-9090-1212-343434343434'
             BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        )
        .unwrap();
        assert_eq!(indexer.do_index(&conn, &files).unwrap(), 0);

        assert_eq!(
            message_rows(&conn),
            vec![("user".to_string(), "old question".to_string())]
        );
        let mtime: f64 = conn
            .query_row("SELECT mtime FROM codex_sync_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mtime, old_mtime);

        conn.execute_batch("DROP TRIGGER fail_insert;").unwrap();
        assert_eq!(indexer.do_index(&conn, &files).unwrap(), 1);
        assert_eq!(message_rows(&conn).len(), 2);
    }

    /// Resuming a Codex session appends to the rollout under its original start
    /// date, so `--days` must filter by mtime rather than by the date directory.
    #[test]
    fn test_days_filter_uses_mtime_not_date_directory() {
        let dir = tempfile::tempdir().unwrap();
        let meta = r#"{"type":"session_meta","payload":{"id":"67676767-8989-0101-2323-454545454545","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#;
        let resumed = write_session_file(dir.path(), "resumed.jsonl", &[meta]);
        let stale = write_session_file(dir.path(), "stale.jsonl", &[meta]);
        set_mtime(
            &stale,
            std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 24 * 60 * 60),
        );

        let indexer = create_indexer(&dir.path().join("sessions"));
        assert_eq!(indexer.find_session_files(Some(1)), vec![resumed]);
        assert_eq!(indexer.find_session_files(None).len(), 2);
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

        let indexer = create_indexer(dir.path());
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

        let indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 2);

        let tool_content: String = conn
            .query_row(
                "SELECT full_content FROM messages WHERE message_type = 'assistant'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            tool_content.contains("[Tool: shell]"),
            "tool content should contain tool name"
        );
        assert!(
            tool_content.contains("ls -la"),
            "tool content should contain command"
        );
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

        let indexer = create_indexer(dir.path());
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

        let indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_extract_uuid_from_filename() {
        let indexer = create_indexer(Path::new("/nonexistent"));

        // Standard UUID in filename
        let uuid = indexer
            .extract_uuid_from_filename("session-abcdef01-2345-6789-abcd-ef0123456789.jsonl");
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

        let indexer = create_indexer(dir.path());
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

        let indexer = create_indexer(dir.path());
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

    #[test]
    fn test_paginated_item_completed_user_message_is_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "paginated-user.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"12121212-3434-5656-7878-909090909090","cwd":"/tmp","timestamp":"2026-10-02T05:05:00Z"}}"#,
                r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\n\ninjected"},{"type":"input_text","text":"<environment_context>\n  <cwd>/tmp</cwd>\n</environment_context>"}]},"timestamp":"2026-10-02T05:05:01Z"}"##,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"本物の指示"}]},"timestamp":"2026-10-02T05:05:02Z"}"#,
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"本物の指示"}]}},"timestamp":"2026-10-02T05:05:02Z"}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"了解しました"}]},"timestamp":"2026-10-02T05:05:03Z"}"#,
            ],
        );

        let indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 2);

        let rows = message_rows(&conn);
        assert_eq!(rows[0], ("user".to_string(), "本物の指示".to_string()));
        assert_eq!(
            rows[1],
            ("assistant".to_string(), "了解しました".to_string())
        );

        let title: String = conn
            .query_row(
                "SELECT conversation_summary FROM conversations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "本物の指示");
    }

    #[test]
    fn test_paginated_response_item_user_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "paginated-fallback.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"23232323-4545-6767-8989-010101010101","cwd":"/tmp","timestamp":"2026-10-02T05:05:00Z"}}"#,
                r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\n\ninjected"}]},"timestamp":"2026-10-02T05:05:01Z"}"##,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/tmp</cwd>\n</environment_context>"}]},"timestamp":"2026-10-02T05:05:02Z"}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"フォールバックの指示"}]},"timestamp":"2026-10-02T05:05:03Z"}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"対応します"}]},"timestamp":"2026-10-02T05:05:04Z"}"#,
            ],
        );

        let indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 2);

        let rows = message_rows(&conn);
        assert_eq!(
            rows[0],
            ("user".to_string(), "フォールバックの指示".to_string())
        );
        assert_eq!(rows[1], ("assistant".to_string(), "対応します".to_string()));

        let title: String = conn
            .query_row(
                "SELECT conversation_summary FROM conversations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "フォールバックの指示");
    }

    #[test]
    fn test_paginated_injected_user_context_is_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "paginated-injected-only.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"34343434-5656-7878-9090-121212121212","cwd":"/tmp","timestamp":"2026-10-02T05:05:00Z"}}"#,
                r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\n\ninjected"}]},"timestamp":"2026-10-02T05:05:01Z"}"##,
                r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>sandbox</permissions instructions>"}]},"timestamp":"2026-10-02T05:05:02Z"}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"了解しました"}]},"timestamp":"2026-10-02T05:05:03Z"}"#,
            ],
        );

        let indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 1);

        let rows = message_rows(&conn);
        assert_eq!(
            rows,
            vec![("assistant".to_string(), "了解しました".to_string())]
        );

        let title: String = conn
            .query_row(
                "SELECT conversation_summary FROM conversations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "Untitled");
    }

    #[test]
    fn test_paginated_item_completed_joins_multiple_text_parts() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();

        let session_file = write_session_file(
            dir.path(),
            "paginated-multipart.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"45454545-6767-8989-0101-232323232323","cwd":"/tmp","timestamp":"2026-10-02T05:05:00Z"}}"#,
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"1行目"},{"type":"text","text":"2行目"}]}},"timestamp":"2026-10-02T05:05:01Z"}"#,
            ],
        );

        let indexer = create_indexer(dir.path());
        let count = indexer.index_session_file(&conn, &session_file).unwrap();
        assert_eq!(count, 1);

        let rows = message_rows(&conn);
        assert_eq!(rows[0], ("user".to_string(), "1行目\n2行目".to_string()));
    }

    const ZST_META: &str = r#"{"type":"session_meta","payload":{"id":"78787878-9090-1212-3434-565656565656","cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#;
    const ZST_USER: &str = r#"{"type":"event_msg","payload":{"type":"user_message","message":"圧縮前の質問"},"timestamp":"2025-01-15T10:00:01Z"}"#;
    const ZST_AGENT: &str = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"compressed answer"},"timestamp":"2025-01-15T10:00:02Z"}"#;

    /// Does what Codex's compression worker does: write `<plain>.zst` with the
    /// plain file's mtime. The plain file is left in place; tests remove it when
    /// they need the post-compression state.
    fn compress_like_codex(plain: &Path) -> PathBuf {
        let data = std::fs::read(plain).unwrap();
        let compressed = PathBuf::from(format!("{}.zst", plain.display()));
        std::fs::write(&compressed, zstd::encode_all(data.as_slice(), 3).unwrap()).unwrap();
        let mtime = plain.metadata().unwrap().modified().unwrap();
        set_mtime(&compressed, mtime);
        compressed
    }

    #[test]
    fn test_find_session_files_includes_compressed_rollouts() {
        let dir = tempfile::tempdir().unwrap();
        let plain = write_session_file(dir.path(), "plain.jsonl", &[ZST_META]);
        let only_zst = write_session_file(dir.path(), "only-zst.jsonl", &[ZST_META]);
        let only_zst = {
            let compressed = compress_like_codex(&only_zst);
            std::fs::remove_file(&only_zst).unwrap();
            compressed
        };
        // Mid-compression (or mid-materialization) both exist; Codex reads the plain one.
        let both = write_session_file(dir.path(), "both.jsonl", &[ZST_META]);
        compress_like_codex(&both);

        let indexer = create_indexer(&dir.path().join("sessions"));
        let mut expected = vec![both.clone(), only_zst.clone(), plain.clone()];
        expected.sort();
        assert_eq!(indexer.find_session_files(None), expected);
        // The background indexer runs with `--days 1`; a recent `.zst` must pass it.
        assert_eq!(indexer.find_session_files(Some(1)), expected);

        // Codex keeps the original mtime on the `.zst`, so `--days` treats it the same.
        set_mtime(
            &only_zst,
            std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 60 * 60),
        );
        let mut recent = vec![both, plain];
        recent.sort();
        assert_eq!(indexer.find_session_files(Some(1)), recent);
    }

    #[test]
    fn test_compressed_rollout_is_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();
        create_sync_table(&conn);

        let plain = write_session_file(
            dir.path(),
            "rollout-zst.jsonl",
            &[ZST_META, ZST_USER, ZST_AGENT],
        );
        let compressed = compress_like_codex(&plain);
        std::fs::remove_file(&plain).unwrap();

        let indexer = create_indexer(&dir.path().join("sessions"));
        let files = indexer.find_session_files(None);
        assert_eq!(indexer.do_index(&conn, &files).unwrap(), 1);
        assert_eq!(
            message_rows(&conn),
            vec![
                ("user".to_string(), "圧縮前の質問".to_string()),
                ("assistant".to_string(), "compressed answer".to_string()),
            ]
        );

        let conversation_file: String = conn
            .query_row("SELECT conversation_file FROM conversations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(conversation_file, compressed.to_string_lossy());
        let sync_keys: Vec<String> = conn
            .prepare("SELECT file_path FROM codex_sync_state")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(sync_keys, vec![plain.to_string_lossy().to_string()]);

        assert_eq!(indexer.do_index(&conn, &files).unwrap(), 0);
    }

    /// Compression changes neither content nor mtime, so a session indexed as
    /// `.jsonl` must not be parsed again once Codex swaps it for `.jsonl.zst`.
    #[test]
    fn test_compression_after_index_does_not_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();
        create_sync_table(&conn);

        let plain = write_session_file(
            dir.path(),
            "rollout-later.jsonl",
            &[ZST_META, ZST_USER, ZST_AGENT],
        );
        let indexer = create_indexer(&dir.path().join("sessions"));
        assert_eq!(
            indexer
                .do_index(&conn, &indexer.find_session_files(None))
                .unwrap(),
            1
        );

        compress_like_codex(&plain);
        std::fs::remove_file(&plain).unwrap();
        assert_eq!(
            indexer
                .do_index(&conn, &indexer.find_session_files(None))
                .unwrap(),
            0
        );
        let sync_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM codex_sync_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sync_rows, 1);
        assert_eq!(message_rows(&conn).len(), 2);
    }

    /// Resuming makes Codex decompress back to `.jsonl` with the original mtime, then
    /// append. Only the append is new content.
    #[test]
    fn test_resumed_rollout_is_reindexed_only_after_append() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();
        create_sync_table(&conn);

        let plain = write_session_file(
            dir.path(),
            "rollout-resumed.jsonl",
            &[ZST_META, ZST_USER, ZST_AGENT],
        );
        let compressed = compress_like_codex(&plain);
        std::fs::remove_file(&plain).unwrap();
        let indexer = create_indexer(&dir.path().join("sessions"));
        assert_eq!(
            indexer
                .do_index(&conn, &indexer.find_session_files(None))
                .unwrap(),
            1
        );

        let original_mtime = compressed.metadata().unwrap().modified().unwrap();
        let decompressed = zstd::decode_all(std::fs::File::open(&compressed).unwrap()).unwrap();
        std::fs::write(&plain, decompressed).unwrap();
        set_mtime(&plain, original_mtime);
        std::fs::remove_file(&compressed).unwrap();
        assert_eq!(
            indexer
                .do_index(&conn, &indexer.find_session_files(None))
                .unwrap(),
            0
        );

        let appended = r#"{"type":"event_msg","payload":{"type":"user_message","message":"再開後の質問"},"timestamp":"2025-01-20T10:00:00Z"}"#;
        write_session_file(
            dir.path(),
            "rollout-resumed.jsonl",
            &[ZST_META, ZST_USER, ZST_AGENT, appended],
        );
        set_mtime(
            &plain,
            original_mtime + std::time::Duration::from_secs(5 * 24 * 60 * 60),
        );
        assert_eq!(
            indexer
                .do_index(&conn, &indexer.find_session_files(None))
                .unwrap(),
            1
        );
        assert_eq!(
            message_rows(&conn).last().unwrap(),
            &("user".to_string(), "再開後の質問".to_string())
        );
        let conversation_file: String = conn
            .query_row("SELECT conversation_file FROM conversations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(conversation_file, plain.to_string_lossy());
    }

    #[test]
    fn test_compressed_rollout_with_invalid_utf8_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();
        let date_dir = dir.path().join("sessions/2025/01/15");
        std::fs::create_dir_all(&date_dir).unwrap();
        let path = date_dir.join("not-utf8.jsonl.zst");
        std::fs::write(&path, zstd::encode_all(&[0xff, 0xfe][..], 3).unwrap()).unwrap();

        let indexer = create_indexer(&dir.path().join("sessions"));
        let err = indexer.index_session_file(&conn, &path).unwrap_err();
        assert!(
            matches!(&err, AppError::Io(e) if e.kind() == std::io::ErrorKind::InvalidData),
            "{err:?}"
        );
    }

    /// Without `payload.id` the session id comes from the file name; it must not pick
    /// up `.jsonl` from `x.jsonl.zst` and split one session into two.
    #[test]
    fn test_fallback_session_id_is_stable_across_compression() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();
        let meta_without_id = r#"{"type":"session_meta","payload":{"cwd":"/tmp","timestamp":"2025-01-15T10:00:00Z"}}"#;
        let plain = write_session_file(
            dir.path(),
            "no-uuid-here.jsonl",
            &[meta_without_id, ZST_USER],
        );
        let compressed = compress_like_codex(&plain);

        let indexer = create_indexer(&dir.path().join("sessions"));
        indexer.index_session_file(&conn, &plain).unwrap();
        indexer.index_session_file(&conn, &compressed).unwrap();
        let session_ids: Vec<String> = conn
            .prepare("SELECT session_id FROM conversations")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(session_ids, vec!["codex:no-uuid-here".to_string()]);
    }

    #[test]
    fn test_corrupt_compressed_rollout_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_test_db();
        create_sync_table(&conn);

        let plain =
            write_session_file(dir.path(), "broken.jsonl", &[ZST_META, ZST_USER, ZST_AGENT]);
        let broken = compress_like_codex(&plain);
        std::fs::remove_file(&plain).unwrap();
        let mut bytes = std::fs::read(&broken).unwrap();
        bytes.truncate(bytes.len() / 2);
        std::fs::write(&broken, bytes).unwrap();

        let indexer = create_indexer(&dir.path().join("sessions"));
        let err = indexer.index_session_file(&conn, &broken).unwrap_err();
        // UnexpectedEof is zstd's "incomplete frame"; reading the bytes as plain text
        // would fail with InvalidData instead.
        assert!(
            matches!(&err, AppError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof),
            "{err:?}"
        );
        assert_eq!(indexer.do_index(&conn, &[broken]).unwrap(), 0);
        let sync_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM codex_sync_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sync_rows, 0);
    }
}
