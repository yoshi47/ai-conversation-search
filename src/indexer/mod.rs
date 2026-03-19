pub mod codex;
pub mod opencode;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Deserialize;

use crate::error::Result;
use crate::git_utils::resolve_repo_root;
use crate::schema::init_schema;
use crate::summarization;

/// Parsed message from a JSONL file.
#[derive(Debug, Clone)]
pub struct Message {
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub is_sidechain: bool,
    pub timestamp: Option<String>,
    pub message_type: String,
    pub content: String,
    pub session_id: Option<String>,
    pub is_meta_conversation: bool,
}

/// Metadata extracted from a conversation JSONL file.
#[derive(Debug)]
pub struct ConversationMeta {
    pub summary: Option<String>,
    pub leaf_uuid: Option<String>,
    pub custom_title: Option<String>,
    pub first_user_message: Option<String>,
}

/// JSONL message entry.
#[derive(Debug, Deserialize)]
struct JsonlEntry {
    uuid: Option<String>,
    #[serde(rename = "parentUuid")]
    parent_uuid: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    timestamp: Option<String>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    message: Option<serde_json::Value>,
    summary: Option<String>,
    #[serde(rename = "leafUuid")]
    leaf_uuid: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
}

/// Content block types in assistant messages.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    text: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

/// Wrapper for sessions-index.json.
#[derive(Debug, Deserialize)]
struct SessionsIndex {
    entries: Vec<SessionsIndexEntry>,
}

/// Entry from sessions-index.json (Claude Code 2026+).
#[derive(Debug, Deserialize)]
struct SessionsIndexEntry {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "firstPrompt")]
    first_prompt: Option<String>,
    summary: Option<String>,
    #[serde(rename = "projectPath")]
    project_path: Option<String>,
}

pub struct ConversationIndexer {
    conn: Connection,
    quiet: bool,
    summarizer_project_hash: Option<String>,
    sessions_index_cache: HashMap<PathBuf, Option<Vec<SessionsIndexEntry>>>,
}

impl ConversationIndexer {
    pub fn new(db_path: &str, quiet: bool) -> Result<Self> {
        let conn = crate::db::connect(db_path, false)?;
        init_schema(&conn)?;

        Ok(Self {
            conn,
            quiet,
            summarizer_project_hash: None,
            sessions_index_cache: HashMap::new(),
        })
    }

    fn log(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{}", msg);
        }
    }

    /// Decode a Claude project directory hash name to a real filesystem path.
    fn decode_project_dir_name(dir_name: &str) -> Option<String> {
        if dir_name.is_empty() || dir_name == "-" {
            return None;
        }

        let raw_parts: Vec<&str> = dir_name.trim_start_matches('-').split('-').collect();
        if raw_parts.is_empty() {
            return None;
        }

        // Handle empty parts from double-dashes
        let mut parts = Vec::new();
        let mut i = 0;
        while i < raw_parts.len() {
            if raw_parts[i].is_empty() && i + 1 < raw_parts.len() {
                parts.push(format!(".{}", raw_parts[i + 1]));
                i += 2;
            } else if !raw_parts[i].is_empty() {
                parts.push(raw_parts[i].to_string());
                i += 1;
            } else {
                i += 1;
            }
        }

        if parts.is_empty() {
            return None;
        }

        Self::try_reconstruct_path(&parts, 0, "")
    }

    fn try_reconstruct_path(parts: &[String], idx: usize, current: &str) -> Option<String> {
        if idx >= parts.len() {
            let path = if current.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", current)
            };
            return if Path::new(&path).exists() {
                Some(path)
            } else {
                None
            };
        }

        let max_consume = std::cmp::min(parts.len() - idx, 5);

        for consume in 1..=max_consume {
            let component_parts = &parts[idx..idx + consume];

            let components_to_try: Vec<String> = if consume == 1 {
                vec![component_parts[0].clone()]
            } else {
                vec![
                    component_parts.join("."),
                    component_parts.join("-"),
                ]
            };

            for component in &components_to_try {
                let candidate = if current.is_empty() {
                    component.clone()
                } else {
                    format!("{}/{}", current, component)
                };

                let candidate_path = format!("/{}", candidate);
                let next_idx = idx + consume;

                if next_idx == parts.len() {
                    if Path::new(&candidate_path).exists() {
                        return Some(candidate_path);
                    }
                } else if Path::new(&candidate_path).is_dir() {
                    if let Some(result) = Self::try_reconstruct_path(parts, next_idx, &candidate) {
                        return Some(result);
                    }
                }
            }
        }

        None
    }

    /// Look up session info from sessions-index.json in the project directory.
    fn lookup_session_info<'a>(
        &'a mut self,
        project_dir: &Path,
        session_id: &str,
    ) -> Option<&'a SessionsIndexEntry> {
        let index_path = project_dir.join("sessions-index.json");
        if !self.sessions_index_cache.contains_key(project_dir) {
            let entries = if index_path.exists() {
                match std::fs::read_to_string(&index_path) {
                    Ok(content) => match serde_json::from_str::<SessionsIndex>(&content) {
                        Ok(idx) => Some(idx.entries),
                        Err(e) => {
                            self.log(&format!(
                                "  Warning: failed to parse {}: {}",
                                index_path.display(),
                                e
                            ));
                            None
                        }
                    },
                    Err(e) => {
                        self.log(&format!(
                            "  Warning: failed to read {}: {}",
                            index_path.display(),
                            e
                        ));
                        None
                    }
                }
            } else {
                None
            };
            self.sessions_index_cache
                .insert(project_dir.to_path_buf(), entries);
        }

        self.sessions_index_cache
            .get(project_dir)
            .and_then(|opt| opt.as_ref())
            .and_then(|entries| entries.iter().find(|e| e.session_id == session_id))
    }

    /// Resolve repo root for a project path, using cache.
    fn resolve_repo_root(
        &self,
        project_path: &str,
        conversation_file: Option<&str>,
    ) -> Option<String> {
        // Check cache
        if let Ok(cached) = self.conn.query_row(
            "SELECT repo_root FROM repo_root_cache WHERE project_path = ?",
            [project_path],
            |row| row.get::<_, Option<String>>(0),
        ) {
            return cached;
        }

        let mut repo_root = None;

        if let Some(conv_file) = conversation_file {
            let conv_path = Path::new(conv_file);
            if let Some(parent) = conv_path.parent() {
                if let Some(dir_name) = parent.file_name() {
                    if let Some(real_path) =
                        Self::decode_project_dir_name(&dir_name.to_string_lossy())
                    {
                        repo_root = resolve_repo_root(&real_path);
                    }
                }
            }
        }

        // Cache successful results
        if let Some(ref root) = repo_root {
            let _ = self.conn.execute(
                "INSERT OR REPLACE INTO repo_root_cache (project_path, repo_root) VALUES (?, ?)",
                rusqlite::params![project_path, root],
            );
        }

        repo_root
    }

    /// Backfill repo_root for existing conversations that don't have one.
    #[allow(dead_code)]
    pub fn backfill_repo_roots(&self) {
        let mut stmt = match self.conn.prepare(
            "SELECT session_id, project_path, conversation_file FROM conversations WHERE repo_root IS NULL AND project_path IS NOT NULL",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };

        let rows: Vec<(String, String, Option<String>)> = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        }) {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        };

        if rows.is_empty() {
            return;
        }

        let mut updated = 0;
        for (session_id, project_path, conv_file) in &rows {
            if let Some(root) = self.resolve_repo_root(project_path, conv_file.as_deref()) {
                let _ = self.conn.execute(
                    "UPDATE conversations SET repo_root = ? WHERE session_id = ?",
                    rusqlite::params![root, session_id],
                );
                updated += 1;
            }
        }

        if updated > 0 {
            self.log(&format!(
                "  Backfilled repo_root for {}/{} conversations",
                updated,
                rows.len()
            ));
        }
    }

    /// Scan ~/.claude/projects for conversation files.
    pub fn scan_conversations(&mut self, days_back: Option<i64>) -> Vec<PathBuf> {
        let projects_dir = match dirs::home_dir() {
            Some(h) => h.join(".claude").join("projects"),
            None => return vec![],
        };

        if !projects_dir.exists() {
            self.log(&format!("Projects directory not found: {}", projects_dir.display()));
            return vec![];
        }

        let cutoff_time = days_back.map(|d| {
            chrono::Local::now() - chrono::TimeDelta::days(d)
        });

        // Get summarizer hash
        let summarizer_hash = self.get_summarizer_project_hash(&projects_dir);

        let mut conversation_files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

        let entries = match std::fs::read_dir(&projects_dir) {
            Ok(e) => e,
            Err(_) => return vec![],
        };

        for entry in entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }

            // Skip summarizer project
            if let Some(ref hash) = summarizer_hash {
                if let Some(name) = project_dir.file_name() {
                    if name.to_string_lossy() == *hash {
                        continue;
                    }
                }
            }

            let dir_entries = match std::fs::read_dir(&project_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for file_entry in dir_entries.flatten() {
                let conv_file = file_entry.path();
                if conv_file.extension().map_or(true, |e| e != "jsonl") {
                    continue;
                }

                // Skip agent files
                if let Some(stem) = conv_file.file_stem() {
                    if stem.to_string_lossy().starts_with("agent-") {
                        continue;
                    }
                }

                // Always get mtime (used for both cutoff filter and sorting)
                let mtime = match conv_file.metadata().and_then(|m| m.modified()) {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                // Check modification time against cutoff
                if let Some(ref cutoff) = cutoff_time {
                    let mtime_dt: chrono::DateTime<chrono::Local> = mtime.into();
                    if mtime_dt < *cutoff {
                        continue;
                    }
                }

                conversation_files.push((conv_file, mtime));
            }
        }

        // Sort by cached mtime descending (no additional stat calls)
        conversation_files.sort_unstable_by(|a, b| b.1.cmp(&a.1));

        conversation_files.into_iter().map(|(p, _)| p).collect()
    }

    fn get_summarizer_project_hash(&mut self, projects_dir: &Path) -> Option<String> {
        if let Some(ref hash) = self.summarizer_project_hash {
            return Some(hash.clone());
        }

        let entries = std::fs::read_dir(projects_dir).ok()?;
        for entry in entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }

            let dir_entries = match std::fs::read_dir(&project_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            let mut checked = 0;
            for file_entry in dir_entries.flatten() {
                if checked >= 5 {
                    break;
                }
                let path = file_entry.path();
                if path.extension().map_or(true, |e| e != "jsonl") {
                    continue;
                }
                checked += 1;

                if let Ok((_, messages)) = self.parse_conversation_file(&path) {
                    if summarization::is_summarizer_conversation(&messages) {
                        let hash = project_dir.file_name()?.to_string_lossy().to_string();
                        self.summarizer_project_hash = Some(hash.clone());
                        self.log(&format!("  Detected summarizer project hash: {}", hash));
                        return Some(hash);
                    }
                }
            }
        }

        None
    }

    /// Parse a conversation JSONL file.
    pub fn parse_conversation_file(
        &self,
        file_path: &Path,
    ) -> Result<(Option<ConversationMeta>, Vec<Message>)> {
        let content = std::fs::read_to_string(file_path)?;
        let mut messages = Vec::new();
        let mut summary_from_jsonl: Option<String> = None;
        let mut leaf_uuid_from_jsonl: Option<String> = None;
        let mut custom_title_found: Option<String> = None;
        let mut first_user_message: Option<String> = None;

        for (line_num, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let entry: JsonlEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(e) => {
                    self.log(&format!(
                        "Error parsing line {} in {}: {}",
                        line_num + 1,
                        file_path.display(),
                        e
                    ));
                    continue;
                }
            };

            // Handle custom-title entries (can appear anywhere in the file)
            if entry.entry_type.as_deref() == Some("custom-title") {
                if let Some(title) = &entry.custom_title {
                    custom_title_found = Some(title.clone());
                }
                continue;
            }

            // First line is the summary (older format)
            if line_num == 0 && entry.entry_type.as_deref() == Some("summary") {
                summary_from_jsonl = entry.summary;
                leaf_uuid_from_jsonl = entry.leaf_uuid;
                continue;
            }

            // Parse message entries
            let uuid = match &entry.uuid {
                Some(u) => u.clone(),
                None => continue,
            };
            let message = match &entry.message {
                Some(m) => m,
                None => continue,
            };

            let message_type = match entry.entry_type.as_deref() {
                Some("user") | Some("assistant") => entry.entry_type.as_ref().unwrap().clone(),
                _ => continue,
            };

            // Extract content
            let msg_content = match message.get("content") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Array(blocks)) => {
                    let mut text_parts = Vec::new();
                    for block in blocks {
                        if let Some(obj) = block.as_object() {
                            match obj.get("type").and_then(|t| t.as_str()) {
                                Some("text") => {
                                    if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                                        text_parts.push(text.to_string());
                                    }
                                }
                                Some("thinking") => {}
                                Some("tool_use") => {
                                    let tool_name = obj
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("unknown");
                                    text_parts.push(format!("[Tool: {}]", tool_name));
                                    if let Some(input) = obj.get("input") {
                                        if let Some(cmd) =
                                            input.get("command").and_then(|c| c.as_str())
                                        {
                                            text_parts.push(cmd.to_string());
                                        }
                                    }
                                }
                                Some("tool_result") => {
                                    text_parts.push("[Tool result]".to_string());
                                }
                                _ => {}
                            }
                        }
                    }
                    text_parts.join("\n")
                }
                _ => String::new(),
            };

            // Capture first user message for summary fallback
            if first_user_message.is_none() && message_type == "user" && !msg_content.is_empty() {
                first_user_message = Some(msg_content.chars().take(100).collect());
            }

            messages.push(Message {
                uuid,
                parent_uuid: entry.parent_uuid,
                is_sidechain: entry.is_sidechain.unwrap_or(false),
                timestamp: entry.timestamp,
                message_type,
                content: msg_content,
                session_id: entry.session_id,
                is_meta_conversation: false,
            });
        }

        let conv_meta = if summary_from_jsonl.is_some()
            || leaf_uuid_from_jsonl.is_some()
            || custom_title_found.is_some()
            || first_user_message.is_some()
        {
            Some(ConversationMeta {
                summary: summary_from_jsonl,
                leaf_uuid: leaf_uuid_from_jsonl,
                custom_title: custom_title_found,
                first_user_message,
            })
        } else {
            None
        };

        Ok((conv_meta, messages))
    }

    /// Calculate depth of each message from root using BFS.
    fn calculate_depth(messages: &[Message]) -> HashMap<String, i32> {
        let mut depths = HashMap::new();
        let children: HashMap<&str, Vec<&str>> = {
            let mut map: HashMap<&str, Vec<&str>> = HashMap::new();
            for m in messages {
                if let Some(ref parent) = m.parent_uuid {
                    map.entry(parent.as_str()).or_default().push(&m.uuid);
                }
            }
            map
        };

        // Find roots
        let mut queue: VecDeque<(&str, i32)> = VecDeque::new();
        for m in messages {
            if m.parent_uuid.is_none() {
                queue.push_back((&m.uuid, 0));
            }
        }

        while let Some((uuid, depth)) = queue.pop_front() {
            depths.insert(uuid.to_string(), depth);
            if let Some(kids) = children.get(uuid) {
                for kid in kids {
                    queue.push_back((kid, depth + 1));
                }
            }
        }

        depths
    }

    /// Mark meta-conversation messages (conversation-search usage).
    fn mark_meta_conversations(messages: &mut [Message]) -> HashSet<String> {
        let mut meta_uuids = HashSet::new();

        // Build owned maps to avoid borrowing messages
        let msg_map: HashMap<String, usize> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| (m.uuid.clone(), i))
            .collect();

        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
        for m in messages.iter() {
            if let Some(ref parent) = m.parent_uuid {
                children_map
                    .entry(parent.clone())
                    .or_default()
                    .push(m.uuid.clone());
            }
        }

        // Collect parent_uuid and content info we need for traversal
        let msg_info: Vec<(String, Option<String>, String, String)> = messages
            .iter()
            .map(|m| (m.uuid.clone(), m.parent_uuid.clone(), m.message_type.clone(), m.content.clone()))
            .collect();

        // Find all messages that use conversation-search
        let search_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                summarization::message_uses_conversation_search(&m.content, &m.message_type)
            })
            .map(|(i, _)| i)
            .collect();

        for idx in search_indices {
            let uuid = msg_info[idx].0.clone();
            meta_uuids.insert(uuid.clone());

            // Walk up ancestor chain
            {
                let mut current_uuid = Some(uuid.clone());
                let mut visited = HashSet::new();
                while let Some(ref cur) = current_uuid {
                    if visited.contains(cur) {
                        break;
                    }
                    visited.insert(cur.clone());

                    if let Some(&i) = msg_map.get(cur) {
                        meta_uuids.insert(cur.clone());

                        if msg_info[i].2 == "user" {
                            let c = msg_info[i].3.trim();
                            if !c.starts_with("[Tool")
                                && !c.starts_with("<command-message>")
                                && !c.starts_with("Base directory")
                            {
                                break;
                            }
                        }
                        current_uuid = msg_info[i].1.clone();
                    } else {
                        break;
                    }
                }
            }

            // Walk down descendant chain
            {
                let mut current_uuid = uuid;
                let mut visited = HashSet::new();
                for _ in 0..20 {
                    let children = match children_map.get(&current_uuid) {
                        Some(c) if !c.is_empty() => c.clone(),
                        _ => break,
                    };

                    let child_uuid = children[0].clone();
                    if visited.contains(&child_uuid) {
                        break;
                    }
                    visited.insert(child_uuid.clone());

                    if let Some(&i) = msg_map.get(&child_uuid) {
                        if msg_info[i].2 == "user" {
                            let c = msg_info[i].3.trim();
                            if !c.starts_with("[Tool")
                                && !c.starts_with("<command-message>")
                                && !c.starts_with("Base directory")
                            {
                                break;
                            }
                        }
                        meta_uuids.insert(child_uuid.clone());
                        current_uuid = child_uuid;
                    } else {
                        break;
                    }
                }
            }
        }

        // Apply meta marking
        for m in messages.iter_mut() {
            if meta_uuids.contains(&m.uuid) {
                m.is_meta_conversation = true;
            }
        }

        meta_uuids
    }

    /// Index a single conversation file.
    pub fn index_conversation(&mut self, file_path: &Path) -> Result<()> {
        // mtime-based skip: avoid re-parsing unchanged files
        let file_mtime = match file_path.metadata().and_then(|m| m.modified()) {
            Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_secs_f64(),
                Err(_) => return self.do_index_conversation(file_path),
            },
            Err(_) => return Ok(()),
        };

        if let Ok(existing_mtime) = self.conn.query_row(
            "SELECT mtime FROM claude_code_sync_state WHERE file_path = ?",
            [file_path.to_string_lossy().as_ref()],
            |row| row.get::<_, f64>(0),
        ) {
            if (existing_mtime - file_mtime).abs() < 0.001 {
                return Ok(());
            }
        }

        let result = self.do_index_conversation(file_path);

        // Only record sync state on success to avoid permanently skipping broken files
        if result.is_ok() {
            let _ = self.conn.execute(
                "INSERT OR REPLACE INTO claude_code_sync_state (file_path, mtime) VALUES (?, ?)",
                rusqlite::params![file_path.to_string_lossy().as_ref(), file_mtime],
            );
        }

        result
    }

    /// Clear all sync state, forcing re-index of all files.
    pub fn clear_sync_state(&self) {
        let _ = self.conn.execute("DELETE FROM claude_code_sync_state", []);
    }

    /// Internal indexing logic for a single conversation file.
    fn do_index_conversation(&mut self, file_path: &Path) -> Result<()> {
        self.log(&format!("Indexing: {}", file_path.display()));

        let (conv_meta, mut messages) = self.parse_conversation_file(file_path)?;

        if messages.is_empty() {
            self.log(&format!("  No messages found in {}", file_path.display()));
            return Ok(());
        }

        // Skip summarizer conversations
        if summarization::is_summarizer_conversation(&messages) {
            self.log("  Skipping automated summarizer conversation");
            return Ok(());
        }

        // Mark meta-conversations
        let meta_uuids = Self::mark_meta_conversations(&mut messages);
        if !meta_uuids.is_empty() {
            self.log(&format!(
                "  Marking {} meta-search messages (~{} pairs)",
                meta_uuids.len(),
                meta_uuids.len() / 2
            ));
        }

        // Extract project path from file location
        let dir_name = file_path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let decoded_path = Self::decode_project_dir_name(&dir_name);
        let decode_succeeded = decoded_path.is_some();
        let mut project_path = decoded_path
            .unwrap_or_else(|| {
                let naive = dir_name.replace('-', "/");
                if naive.starts_with('/') { naive } else { format!("/{}", naive) }
            });

        // Get session ID
        let session_id = match messages[0].session_id.as_ref() {
            Some(s) => s.clone(),
            None => {
                self.log(&format!("  No session_id found in {}", file_path.display()));
                return Ok(());
            }
        };

        // Calculate depths
        let depths = Self::calculate_depth(&messages);

        // Resolve conversation summary using priority chain:
        // 1. JSONL summary line (older format)
        // 2. JSONL custom-title line
        // 3. sessions-index.json summary
        // 4. sessions-index.json firstPrompt (truncated to 100 chars)
        // 5. First user message (truncated to 100 chars)
        // 6. "Untitled conversation"
        let project_dir = file_path.parent().map(|p| p.to_path_buf());
        let session_info = project_dir.as_ref().and_then(|pd| {
            self.lookup_session_info(pd, &session_id)
                .map(|e| (e.summary.clone(), e.first_prompt.clone(), e.project_path.clone()))
        });
        let (si_summary, si_first_prompt, si_project_path) = match session_info {
            Some((s, fp, pp)) => (s, fp, pp),
            None => (None, None, None),
        };

        // Use sessions-index.json projectPath as fallback when decode failed
        if !decode_succeeded {
            if let Some(ref pp) = si_project_path {
                if !pp.is_empty() {
                    project_path = pp.clone();
                }
            }
        }

        let conversation_summary = conv_meta
            .as_ref()
            .and_then(|m| m.summary.clone())
            .or_else(|| conv_meta.as_ref().and_then(|m| m.custom_title.clone()))
            .or(si_summary)
            .or_else(|| si_first_prompt.map(|fp| fp.chars().take(100).collect()))
            .or_else(|| conv_meta.as_ref().and_then(|m| m.first_user_message.clone()))
            .unwrap_or_else(|| "Untitled conversation".to_string());

        // Check if already indexed
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT indexed_at FROM conversations WHERE session_id = ?",
                [&session_id],
                |row| row.get(0),
            )
            .ok();

        let is_update;
        let messages_to_insert;

        if let Some(ref indexed_at) = existing {
            self.log(&format!(
                "  Already indexed at {}, checking for new messages...",
                indexed_at
            ));

            let mut stmt = self
                .conn
                .prepare("SELECT message_uuid FROM messages WHERE session_id = ?")?;
            let existing_uuids: HashSet<String> = stmt
                .query_map([&session_id], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            let new_messages: Vec<&Message> = messages
                .iter()
                .filter(|m| !existing_uuids.contains(&m.uuid))
                .collect();

            if new_messages.is_empty() {
                self.log("  No new messages, skipping");
                return Ok(());
            }

            self.log(&format!(
                "  Found {} new messages (total: {})",
                new_messages.len(),
                messages.len()
            ));

            // Update conversation metadata (including summary and project_path)
            self.conn.execute(
                "UPDATE conversations SET last_message_at = ?, message_count = ?, leaf_message_uuid = ?, conversation_summary = ?, project_path = ?, indexed_at = CURRENT_TIMESTAMP WHERE session_id = ?",
                rusqlite::params![
                    messages.last().and_then(|m| m.timestamp.as_ref()),
                    (existing_uuids.len() + new_messages.len()) as i64,
                    conv_meta.as_ref().and_then(|m| m.leaf_uuid.as_ref()),
                    conversation_summary,
                    project_path,
                    session_id,
                ],
            )?;

            is_update = true;
            messages_to_insert = new_messages.into_iter().cloned().collect::<Vec<_>>();
        } else {
            // New conversation
            let root_message = messages
                .iter()
                .find(|m| m.parent_uuid.is_none())
                .unwrap_or(&messages[0]);
            let repo_root =
                self.resolve_repo_root(&project_path, Some(&file_path.to_string_lossy()));

            self.conn.execute(
                "INSERT INTO conversations (session_id, project_path, repo_root, conversation_file, root_message_uuid, leaf_message_uuid, conversation_summary, first_message_at, last_message_at, message_count) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    session_id,
                    project_path,
                    repo_root,
                    file_path.to_string_lossy(),
                    root_message.uuid,
                    conv_meta.as_ref().and_then(|m| m.leaf_uuid.as_ref()),
                    conversation_summary,
                    messages[0].timestamp,
                    messages.last().and_then(|m| m.timestamp.as_ref()),
                    messages.len() as i64,
                ],
            )?;

            is_update = false;
            messages_to_insert = messages.clone();
        }

        // Classify tool noise
        let tool_noise_uuids: HashSet<String> = messages_to_insert
            .iter()
            .filter(|m| summarization::is_tool_noise(&m.content, &m.message_type))
            .map(|m| m.uuid.clone())
            .collect();

        // Insert messages
        {
            let tx = self.conn.unchecked_transaction()?;
            for message in &messages_to_insert {
                tx.execute(
                    "INSERT INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    rusqlite::params![
                        message.uuid,
                        session_id,
                        message.parent_uuid,
                        message.is_sidechain,
                        depths.get(&message.uuid).unwrap_or(&0),
                        message.timestamp,
                        message.message_type,
                        project_path,
                        file_path.to_string_lossy(),
                        message.content,
                        message.is_meta_conversation,
                        tool_noise_uuids.contains(&message.uuid),
                    ],
                )?;
            }
            tx.commit()?;
        }

        if !tool_noise_uuids.is_empty() {
            self.log(&format!(
                "  Marked {} messages as tool noise",
                tool_noise_uuids.len()
            ));
        }

        if is_update {
            self.log(&format!("  Added {} new messages", messages_to_insert.len()));
        } else {
            self.log(&format!("  Indexed {} messages", messages_to_insert.len()));
        }

        Ok(())
    }

    /// Index all conversations from the last N days.
    #[allow(dead_code)]
    pub fn index_all(&mut self, days_back: Option<i64>) -> Result<()> {
        let files = self.scan_conversations(days_back);
        self.log(&format!("Found {} conversation files to index", files.len()));

        let total = files.len();
        for (i, file_path) in files.into_iter().enumerate() {
            self.log(&format!("\n[{}/{}]", i + 1, total));
            if let Err(e) = self.index_conversation(&file_path) {
                self.log(&format!("  Error indexing {}: {}", file_path.display(), e));
            }
        }

        self.backfill_repo_roots();
        self.log("\nIndexing complete!");
        Ok(())
    }

    /// Get a mutable reference to the connection (for external indexers).
    #[allow(dead_code)]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_indexer() -> (tempfile::TempDir, ConversationIndexer) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let indexer = ConversationIndexer::new(&db_path, true).unwrap();
        (dir, indexer)
    }

    fn write_temp_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
        file
    }

    #[test]
    fn test_parse_conversation_file_basic() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[
            r#"{"type":"summary","summary":"Test conversation","leafUuid":"uuid3"}"#,
            r#"{"uuid":"uuid1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess1","message":{"role":"user","content":"Hello world"}}"#,
            r#"{"uuid":"uuid2","parentUuid":"uuid1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sess1","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"}]}}"#,
            r#"{"uuid":"uuid3","parentUuid":"uuid2","isSidechain":false,"timestamp":"2025-01-15T10:02:00Z","type":"user","sessionId":"sess1","message":{"role":"user","content":"How are you?"}}"#,
        ]);

        let (summary, messages) = indexer.parse_conversation_file(file.path()).unwrap();

        // Verify conversation meta
        let summary = summary.unwrap();
        assert_eq!(summary.summary.as_deref(), Some("Test conversation"));
        assert_eq!(summary.leaf_uuid.as_deref(), Some("uuid3"));
        assert!(summary.custom_title.is_none());
        assert_eq!(summary.first_user_message.as_deref(), Some("Hello world"));

        // Verify messages
        assert_eq!(messages.len(), 3);

        assert_eq!(messages[0].uuid, "uuid1");
        assert_eq!(messages[0].message_type, "user");
        assert_eq!(messages[0].content, "Hello world");
        assert!(messages[0].parent_uuid.is_none());

        assert_eq!(messages[1].uuid, "uuid2");
        assert_eq!(messages[1].message_type, "assistant");
        assert_eq!(messages[1].content, "Hi there!");
        assert_eq!(messages[1].parent_uuid.as_deref(), Some("uuid1"));

        assert_eq!(messages[2].uuid, "uuid3");
        assert_eq!(messages[2].message_type, "user");
        assert_eq!(messages[2].content, "How are you?");
    }

    #[test]
    fn test_parse_content_blocks_text() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[
            r#"{"type":"summary","summary":"test","leafUuid":"u1"}"#,
            r#"{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"First part"},{"type":"text","text":"Second part"}]}}"#,
        ]);

        let (_, messages) = indexer.parse_conversation_file(file.path()).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "First part\nSecond part");
    }

    #[test]
    fn test_parse_content_blocks_tool_use() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[
            r#"{"type":"summary","summary":"test","leafUuid":"u1"}"#,
            r#"{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/test.txt"}}]}}"#,
        ]);

        let (_, messages) = indexer.parse_conversation_file(file.path()).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("[Tool: Read]"));
    }

    #[test]
    fn test_parse_content_blocks_tool_result() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[
            r#"{"type":"summary","summary":"test","leafUuid":"u1"}"#,
            r#"{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"tool_result"},{"type":"text","text":"Done"}]}}"#,
        ]);

        let (_, messages) = indexer.parse_conversation_file(file.path()).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("[Tool result]"));
        assert!(messages[0].content.contains("Done"));
    }

    #[test]
    fn test_parse_content_blocks_thinking() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[
            r#"{"type":"summary","summary":"test","leafUuid":"u1"}"#,
            r#"{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"thinking","text":"internal thoughts"},{"type":"text","text":"Visible response"}]}}"#,
        ]);

        let (_, messages) = indexer.parse_conversation_file(file.path()).unwrap();
        assert_eq!(messages.len(), 1);
        // Thinking blocks should be skipped
        assert!(!messages[0].content.contains("internal thoughts"));
        assert_eq!(messages[0].content, "Visible response");
    }

    #[test]
    fn test_calculate_depth_linear() {
        let messages = vec![
            Message {
                uuid: "a".to_string(),
                parent_uuid: None,
                is_sidechain: false,
                timestamp: Some("2025-01-01T00:00:00Z".to_string()),
                message_type: "user".to_string(),
                content: "msg a".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
            Message {
                uuid: "b".to_string(),
                parent_uuid: Some("a".to_string()),
                is_sidechain: false,
                timestamp: Some("2025-01-01T00:01:00Z".to_string()),
                message_type: "assistant".to_string(),
                content: "msg b".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
            Message {
                uuid: "c".to_string(),
                parent_uuid: Some("b".to_string()),
                is_sidechain: false,
                timestamp: Some("2025-01-01T00:02:00Z".to_string()),
                message_type: "user".to_string(),
                content: "msg c".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
        ];

        let depths = ConversationIndexer::calculate_depth(&messages);
        assert_eq!(depths["a"], 0);
        assert_eq!(depths["b"], 1);
        assert_eq!(depths["c"], 2);
    }

    #[test]
    fn test_calculate_depth_branching() {
        // Tree:  root -> child1 -> grandchild
        //             -> child2
        let messages = vec![
            Message {
                uuid: "root".to_string(),
                parent_uuid: None,
                is_sidechain: false,
                timestamp: Some("2025-01-01T00:00:00Z".to_string()),
                message_type: "user".to_string(),
                content: "root".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
            Message {
                uuid: "child1".to_string(),
                parent_uuid: Some("root".to_string()),
                is_sidechain: false,
                timestamp: Some("2025-01-01T00:01:00Z".to_string()),
                message_type: "assistant".to_string(),
                content: "child1".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
            Message {
                uuid: "child2".to_string(),
                parent_uuid: Some("root".to_string()),
                is_sidechain: true,
                timestamp: Some("2025-01-01T00:02:00Z".to_string()),
                message_type: "assistant".to_string(),
                content: "child2".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
            Message {
                uuid: "grandchild".to_string(),
                parent_uuid: Some("child1".to_string()),
                is_sidechain: false,
                timestamp: Some("2025-01-01T00:03:00Z".to_string()),
                message_type: "user".to_string(),
                content: "grandchild".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
        ];

        let depths = ConversationIndexer::calculate_depth(&messages);
        assert_eq!(depths["root"], 0);
        assert_eq!(depths["child1"], 1);
        assert_eq!(depths["child2"], 1);
        assert_eq!(depths["grandchild"], 2);
    }

    #[test]
    fn test_mark_meta_conversations() {
        // Create a chain: user_ask -> assistant_uses_search -> user_followup
        // The assistant message uses conversation-search, so it and its ancestors should be marked.
        let mut messages = vec![
            Message {
                uuid: "u1".to_string(),
                parent_uuid: None,
                is_sidechain: false,
                timestamp: Some("2025-01-01T00:00:00Z".to_string()),
                message_type: "user".to_string(),
                content: "Find my old conversation about Rust".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
            Message {
                uuid: "a1".to_string(),
                parent_uuid: Some("u1".to_string()),
                is_sidechain: false,
                timestamp: Some("2025-01-01T00:01:00Z".to_string()),
                message_type: "assistant".to_string(),
                content: "[Tool: Bash] ai-conversation-search search rust".to_string(),
                session_id: Some("s1".to_string()),
                is_meta_conversation: false,
            },
        ];

        let meta_uuids = ConversationIndexer::mark_meta_conversations(&mut messages);

        // Both messages should be marked as meta
        assert!(meta_uuids.contains("a1"), "assistant message should be meta");
        assert!(meta_uuids.contains("u1"), "user ancestor should be meta");
        assert!(messages[0].is_meta_conversation);
        assert!(messages[1].is_meta_conversation);
    }

    #[test]
    fn test_decode_project_path() {
        // Non-existent path should return None
        let result = ConversationIndexer::decode_project_dir_name("nonexistent-path-xyz");
        assert!(result.is_none());

        // Empty and dash-only should return None
        assert!(ConversationIndexer::decode_project_dir_name("").is_none());
        assert!(ConversationIndexer::decode_project_dir_name("-").is_none());
    }

    #[test]
    fn test_parse_empty_file() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[]);

        let (summary, messages) = indexer.parse_conversation_file(file.path()).unwrap();
        assert!(summary.is_none());
        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_corrupt_lines() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[
            r#"{"type":"summary","summary":"test","leafUuid":"u2"}"#,
            r#"this is not valid json at all"#,
            r#"{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"s1","message":{"role":"user","content":"Valid message"}}"#,
            r#"{"broken: json"#,
            r#"{"uuid":"u2","parentUuid":"u1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"s1","message":{"role":"assistant","content":"Also valid"}}"#,
        ]);

        let (summary, messages) = indexer.parse_conversation_file(file.path()).unwrap();

        // Summary should be parsed
        assert!(summary.is_some());
        assert_eq!(summary.unwrap().summary.as_deref(), Some("test"));

        // Only valid message lines should be parsed (corrupt lines skipped)
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "Valid message");
        assert_eq!(messages[1].content, "Also valid");
    }

    #[test]
    fn test_parse_custom_title() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[
            r#"{"uuid":"uuid1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess1","message":{"role":"user","content":"Hello world"}}"#,
            r#"{"uuid":"uuid2","parentUuid":"uuid1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sess1","message":{"role":"assistant","content":"Hi!"}}"#,
            r#"{"type":"custom-title","customTitle":"My Custom Title"}"#,
        ]);

        let (meta, messages) = indexer.parse_conversation_file(file.path()).unwrap();
        assert_eq!(messages.len(), 2);

        let meta = meta.unwrap();
        assert!(meta.summary.is_none()); // no summary line
        assert_eq!(meta.custom_title.as_deref(), Some("My Custom Title"));
        assert_eq!(meta.first_user_message.as_deref(), Some("Hello world"));
    }

    #[test]
    fn test_parse_first_user_message_truncation() {
        let (_dir, indexer) = create_test_indexer();
        let long_message = "a".repeat(200);
        let line = format!(
            r#"{{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"s1","message":{{"role":"user","content":"{}"}}}}"#,
            long_message
        );
        let file = write_temp_jsonl(&[&line]);

        let (meta, _) = indexer.parse_conversation_file(file.path()).unwrap();
        let meta = meta.unwrap();
        assert_eq!(meta.first_user_message.as_ref().unwrap().len(), 100);
    }

    #[test]
    fn test_parse_no_summary_no_custom_title_has_first_user_message() {
        let (_dir, indexer) = create_test_indexer();
        let file = write_temp_jsonl(&[
            r#"{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"s1","message":{"role":"user","content":"First question"}}"#,
            r#"{"uuid":"u2","parentUuid":"u1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"s1","message":{"role":"assistant","content":"Answer"}}"#,
        ]);

        let (meta, _) = indexer.parse_conversation_file(file.path()).unwrap();
        let meta = meta.unwrap();
        assert!(meta.summary.is_none());
        assert!(meta.custom_title.is_none());
        assert_eq!(meta.first_user_message.as_deref(), Some("First question"));
    }
}
