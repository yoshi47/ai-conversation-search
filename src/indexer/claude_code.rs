use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Deserialize;

use crate::error::Result;
use crate::git_utils::resolve_repo_root;
use crate::schema::init_schema;
use crate::summarization;

use super::{ConversationMeta, Message};

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

/// Count conversation files on disk without needing a DB connection.
/// Used by `status` command and unindexed file warnings.
pub fn count_conversation_files_on_disk() -> usize {
    let projects_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude").join("projects"),
        None => {
            eprintln!("Warning: could not determine home directory; file count unavailable");
            return 0;
        }
    };

    if !projects_dir.exists() {
        return 0;
    }

    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Warning: could not read {}: {}", projects_dir.display(), e);
            return 0;
        }
    };

    let mut count = 0;
    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
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
            if let Some(stem) = conv_file.file_stem() {
                if stem.to_string_lossy().starts_with("agent-") {
                    continue;
                }
            }
            count += 1;
        }
    }

    count
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
                vec![component_parts.join("."), component_parts.join("-")]
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
        // Check DB cache first
        if let Ok(cached) = self.conn.query_row(
            "SELECT repo_root FROM repo_root_cache WHERE project_path = ?",
            [project_path],
            |row| row.get::<_, Option<String>>(0),
        ) {
            return cached;
        }

        // Claude Code stores project dirs as hashed names; decode from conversation file path
        let repo_root = conversation_file.and_then(|conv_file| {
            let conv_path = Path::new(conv_file);
            let parent = conv_path.parent()?;
            let dir_name = parent.file_name()?;
            let real_path = Self::decode_project_dir_name(&dir_name.to_string_lossy())?;
            resolve_repo_root(&real_path)
        });

        // Cache result
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

    /// Discover all Claude project directories to scan.
    /// Auto-discovers ~/.claude/projects and ~/.claude-*/projects,
    /// plus any directories specified in CONVERSATION_SEARCH_EXTRA_DIRS (colon-separated).
    fn discover_project_dirs(&self) -> Vec<PathBuf> {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => {
                self.log("Warning: could not determine home directory");
                return vec![];
            }
        };

        let mut dirs = Vec::new();

        // Auto-discover: ~/.claude/projects, ~/.claude-*/projects
        match std::fs::read_dir(&home) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            self.log(&format!(
                                "Warning: failed to read entry in {}: {}",
                                home.display(),
                                e
                            ));
                            continue;
                        }
                    };
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if (name_str == ".claude" || name_str.starts_with(".claude-"))
                        && entry.path().is_dir()
                    {
                        let projects = entry.path().join("projects");
                        if projects.is_dir() {
                            dirs.push(projects);
                        }
                    }
                }
            }
            Err(e) => {
                self.log(&format!(
                    "Warning: failed to read home directory {}: {}",
                    home.display(),
                    e
                ));
            }
        }

        // Extra dirs from env var (colon-separated, supports ~ expansion)
        if let Ok(extra) = std::env::var("CONVERSATION_SEARCH_EXTRA_DIRS") {
            for dir in extra.split(':').filter(|s| !s.is_empty()) {
                let expanded = if dir == "~" {
                    home.clone()
                } else if dir.starts_with("~/") {
                    home.join(&dir[2..])
                } else {
                    PathBuf::from(dir)
                };
                if expanded.is_dir() {
                    dirs.push(expanded);
                } else {
                    self.log(&format!(
                        "Warning: CONVERSATION_SEARCH_EXTRA_DIRS entry '{}' (resolved to '{}') is not a directory, skipping",
                        dir, expanded.display()
                    ));
                }
            }
        }

        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Scan Claude project directories for conversation files.
    /// Auto-discovers multiple profiles (e.g. ~/.claude, ~/.claude-personal).
    pub fn scan_conversations(&mut self, days_back: Option<i64>) -> Vec<PathBuf> {
        let project_dirs = self.discover_project_dirs();

        if project_dirs.is_empty() {
            self.log("No Claude project directories found");
            return vec![];
        }

        self.log(&format!(
            "Scanning {} project directories: {}",
            project_dirs.len(),
            project_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));

        let cutoff_time = days_back.map(|d| chrono::Local::now() - chrono::TimeDelta::days(d));

        let mut conversation_files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

        for projects_dir in &project_dirs {
            // Reset cached summarizer hash for each profile directory
            self.summarizer_project_hash = None;
            let summarizer_hash = self.get_summarizer_project_hash(projects_dir);

            let entries = match std::fs::read_dir(projects_dir) {
                Ok(e) => e,
                Err(e) => {
                    self.log(&format!(
                        "Warning: failed to read project directory {}: {}",
                        projects_dir.display(),
                        e
                    ));
                    continue;
                }
            };

            for entry in entries.flatten() {
                let project_dir = entry.path();
                if !project_dir.is_dir() {
                    continue;
                }

                // Skip summarizer project
                if summarizer_hash.as_ref().is_some_and(|hash| {
                    project_dir
                        .file_name()
                        .map_or(false, |name| name.to_string_lossy() == *hash)
                }) {
                    continue;
                }

                let dir_entries = match std::fs::read_dir(&project_dir) {
                    Ok(e) => e,
                    Err(e) => {
                        self.log(&format!(
                            "Warning: failed to read project subdirectory {}: {}",
                            project_dir.display(),
                            e
                        ));
                        continue;
                    }
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

    /// Parse a conversation JSONL file without requiring a DB/indexer.
    ///
    /// Malformed lines are skipped without logging; the number of skipped
    /// lines is returned so callers can surface partial reads. Use
    /// `parse_conversation_file` when per-line diagnostics are needed.
    pub(crate) fn parse_conversation_file_raw(
        file_path: &Path,
    ) -> Result<(Option<ConversationMeta>, Vec<Message>, usize)> {
        Self::parse_conversation_file_with_logger(file_path, None)
    }

    fn parse_conversation_file_with_logger(
        file_path: &Path,
        log_fn: Option<&dyn Fn(&str)>,
    ) -> Result<(Option<ConversationMeta>, Vec<Message>, usize)> {
        let content = std::fs::read_to_string(file_path)?;
        let mut messages = Vec::new();
        let mut skipped_lines = 0usize;
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
                    skipped_lines += 1;
                    if let Some(log) = log_fn {
                        log(&format!(
                            "Error parsing line {} in {}: {}",
                            line_num + 1,
                            file_path.display(),
                            e
                        ));
                    }
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

        Ok((conv_meta, messages, skipped_lines))
    }

    /// Parse a conversation JSONL file.
    pub fn parse_conversation_file(
        &self,
        file_path: &Path,
    ) -> Result<(Option<ConversationMeta>, Vec<Message>)> {
        let log = |msg: &str| self.log(msg);
        let (conv_meta, messages, _skipped_lines) =
            Self::parse_conversation_file_with_logger(file_path, Some(&log))?;
        Ok((conv_meta, messages))
    }

    /// Calculate depth of each message from root using BFS.
    ///
    /// Messages not reachable from a true root (`parent_uuid == None`) are
    /// omitted from the result; callers must choose a fallback depth for them.
    pub(crate) fn calculate_depth(messages: &[Message]) -> HashMap<String, i32> {
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
            .map(|m| {
                (
                    m.uuid.clone(),
                    m.parent_uuid.clone(),
                    m.message_type.clone(),
                    m.content.clone(),
                )
            })
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
        // mtime-based skip: avoid re-parsing unchanged files. If mtime is
        // unavailable (permission denied, pre-1970 clock, deleted-mid-scan)
        // we still attempt indexing — file_mtime = None signals "do not write
        // sync_state", which forces a re-attempt on the next run. Logging the
        // reason here prevents the previous silent-skip behavior where an
        // entire project could become permanently unindexable on a transient
        // stat error.
        let file_mtime = match file_path.metadata().and_then(|m| m.modified()) {
            Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => Some(d.as_secs_f64()),
                Err(e) => {
                    // Use eprintln (not self.log) so users under --quiet still
                    // see this — a missing mtime means we lose the ability to
                    // skip on re-index and silently retry the same file every
                    // single run. That deserves to surface.
                    eprintln!(
                        "Warning: cannot read mtime for {} (clock before UNIX epoch: {}), will re-attempt next run",
                        file_path.display(),
                        e
                    );
                    None
                }
            },
            Err(e) => {
                eprintln!(
                    "Warning: cannot stat {}: {} — will re-attempt next run",
                    file_path.display(),
                    e
                );
                None
            }
        };

        if let Some(mtime) = file_mtime {
            if let Ok(existing_mtime) = self.conn.query_row(
                "SELECT mtime FROM claude_code_sync_state WHERE file_path = ?",
                [file_path.to_string_lossy().as_ref()],
                |row| row.get::<_, f64>(0),
            ) {
                if (existing_mtime - mtime).abs() < 0.001 {
                    return Ok(());
                }
            }
        }

        // do_index_conversation writes sync_state inside its single transaction,
        // so a failure leaves no orphaned conversation row AND no sync_state row,
        // guaranteeing the file is retried on the next index run.
        self.do_index_conversation(file_path, file_mtime)
    }

    /// Internal indexing logic for a single conversation file.
    ///
    /// `file_mtime` is recorded in `claude_code_sync_state` on success (inside
    /// the same transaction as the conversation/messages writes). Pass `None`
    /// when the mtime is unavailable; the index will still proceed but won't
    /// be skipped on the next run.
    fn do_index_conversation(&mut self, file_path: &Path, file_mtime: Option<f64>) -> Result<()> {
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
        let mut project_path = decoded_path.unwrap_or_else(|| {
            let naive = dir_name.replace('-', "/");
            if naive.starts_with('/') {
                naive
            } else {
                format!("/{}", naive)
            }
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
            self.lookup_session_info(pd, &session_id).map(|e| {
                (
                    e.summary.clone(),
                    e.first_prompt.clone(),
                    e.project_path.clone(),
                )
            })
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
            .or_else(|| {
                conv_meta
                    .as_ref()
                    .and_then(|m| m.first_user_message.clone())
            })
            .unwrap_or_else(|| "Untitled conversation".to_string());

        // Resolve repo_root before opening the write transaction — it shells
        // out to `git` and writes to `repo_root_cache` via self.conn, which
        // would conflict with the outer transaction we're about to open.
        let repo_root = self.resolve_repo_root(&project_path, Some(&file_path.to_string_lossy()));

        // Pick root message (used only on the INSERT path).
        let root_message_uuid = messages
            .iter()
            .find(|m| m.parent_uuid.is_none())
            .map(|m| m.uuid.clone())
            .unwrap_or_else(|| messages[0].uuid.clone());

        // -------- single write transaction --------
        // Conversation upsert, messages insert, and sync_state update all live
        // in this tx so failure rolls back atomically and we never end up with
        // an orphan conversations row (was the cause of the message_count > 0
        // but messages = 0 bug).
        let tx = self.conn.unchecked_transaction()?;

        // Check if already indexed
        let existing: Option<String> = tx
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

            // Read the existing UUIDs with full error propagation. Silently
            // dropping a row here would make that message look "new" on the
            // next pass and re-insert it, inflating duplicate counts. Worse,
            // the message could end up attributed to a different session.
            let existing_uuids: HashSet<String> = {
                let mut stmt =
                    tx.prepare("SELECT message_uuid FROM messages WHERE session_id = ?")?;
                let rows: rusqlite::Result<HashSet<String>> =
                    stmt.query_map([&session_id], |row| row.get(0))?.collect();
                rows?
            };

            let new_messages: Vec<&Message> = messages
                .iter()
                .filter(|m| !existing_uuids.contains(&m.uuid))
                .collect();

            if new_messages.is_empty() {
                self.log("  No new messages, skipping");
                // Still record sync_state so we don't re-parse unchanged file.
                if let Some(mtime) = file_mtime {
                    tx.execute(
                        "INSERT OR REPLACE INTO claude_code_sync_state (file_path, mtime) VALUES (?, ?)",
                        rusqlite::params![file_path.to_string_lossy().as_ref(), mtime],
                    )?;
                }
                tx.commit()?;
                return Ok(());
            }

            self.log(&format!(
                "  Found {} new messages (total: {})",
                new_messages.len(),
                messages.len()
            ));

            // Update conversation metadata. message_count is set to 0 here as a
            // placeholder; the real value is computed via COUNT(*) after the
            // message INSERTs below (since INSERT OR IGNORE may skip rows whose
            // UUIDs already exist under a different session_id — Claude Code
            // resume sessions re-emit parent messages).
            tx.execute(
                "UPDATE conversations SET last_message_at = ?, message_count = 0, leaf_message_uuid = ?, conversation_summary = ?, project_path = ?, indexed_at = CURRENT_TIMESTAMP WHERE session_id = ?",
                rusqlite::params![
                    messages.last().and_then(|m| m.timestamp.as_ref()),
                    conv_meta.as_ref().and_then(|m| m.leaf_uuid.as_ref()),
                    conversation_summary,
                    project_path,
                    session_id,
                ],
            )?;

            is_update = true;
            messages_to_insert = new_messages.into_iter().cloned().collect::<Vec<_>>();
        } else {
            // New conversation. message_count placeholder is 0; corrected after
            // message INSERTs via COUNT(*) (see comment in the UPDATE branch).
            tx.execute(
                "INSERT INTO conversations (session_id, project_path, repo_root, conversation_file, root_message_uuid, leaf_message_uuid, conversation_summary, first_message_at, last_message_at, message_count) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
                rusqlite::params![
                    session_id,
                    project_path,
                    repo_root,
                    file_path.to_string_lossy(),
                    root_message_uuid,
                    conv_meta.as_ref().and_then(|m| m.leaf_uuid.as_ref()),
                    conversation_summary,
                    messages[0].timestamp,
                    messages.last().and_then(|m| m.timestamp.as_ref()),
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

        // Insert messages. INSERT OR IGNORE handles the legitimate case of
        // Claude Code resume sessions re-emitting parent messages with the
        // same global message_uuid under a new session_id. Message content is
        // immutable, so keeping the original row and skipping the duplicate is
        // correct. Do NOT replace this with plain INSERT — it will fail on
        // PRIMARY KEY violation and roll the whole transaction back, which is
        // exactly the bug we're fixing here.
        for message in &messages_to_insert {
            tx.execute(
                "INSERT OR IGNORE INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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

        // Recompute message_count from the actual rows in messages. This is
        // the single source of truth and correctly accounts for OR IGNORE skips.
        let actual_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?",
            [&session_id],
            |row| row.get(0),
        )?;

        // Edge case: a fresh INSERT where every parsed message UUID collided
        // with an existing row in another session (Claude Code resume where
        // the sister session was indexed first). The content is already
        // searchable under the sister session_id; keeping a 0-message conv
        // row here would be misleading metadata that show up in `list` but
        // be invisible to content search. Drop it. We still write sync_state
        // below so we don't re-parse this file on every subsequent index run.
        let dropped_ghost = !is_update && actual_count == 0;
        if dropped_ghost {
            tx.execute(
                "DELETE FROM conversations WHERE session_id = ?",
                [&session_id],
            )?;
            self.log(&format!(
                "  All {} messages already attributed to a sister session — dropping empty conversation row",
                messages_to_insert.len()
            ));
        } else {
            tx.execute(
                "UPDATE conversations SET message_count = ? WHERE session_id = ?",
                rusqlite::params![actual_count, &session_id],
            )?;
        }

        // Record sync_state inside the same tx so it never gets ahead of the
        // actual data on disk.
        if let Some(mtime) = file_mtime {
            tx.execute(
                "INSERT OR REPLACE INTO claude_code_sync_state (file_path, mtime) VALUES (?, ?)",
                rusqlite::params![file_path.to_string_lossy().as_ref(), mtime],
            )?;
        }

        tx.commit()?;

        if !tool_noise_uuids.is_empty() {
            self.log(&format!(
                "  Marked {} messages as tool noise",
                tool_noise_uuids.len()
            ));
        }

        if dropped_ghost {
            // Already logged the drop above; suppress the misleading
            // "Indexed N messages" footer.
        } else if is_update {
            self.log(&format!(
                "  Added {} new messages",
                messages_to_insert.len()
            ));
        } else {
            self.log(&format!("  Indexed {} messages", messages_to_insert.len()));
        }

        Ok(())
    }

    /// Repair conversations rows whose message rows are missing.
    ///
    /// Background: a pre-0.12.1 bug could leave `conversations` rows with
    /// `message_count > 0` but no rows in `messages` (failed message INSERT
    /// rolled back while the conversations row was already committed). Such
    /// rows are invisible to content search even though `list` returns them.
    ///
    /// This deletes the orphaned conversations rows plus their sync_state
    /// entries, so the next index run re-parses the JSONL from scratch. Now
    /// that the indexer is transactional, freshly-indexed conversations cannot
    /// re-enter this state.
    ///
    /// Returns the number of conversations repaired.
    pub fn repair_orphan_conversations(&mut self) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;

        // Collect orphan session_ids and their conversation_file paths.
        // Full error propagation (not filter_map) so a schema drift surfaces
        // immediately instead of silently under-reporting.
        let orphans: Vec<(String, Option<String>)> = {
            let mut stmt = tx.prepare(
                "SELECT c.session_id, c.conversation_file
                 FROM conversations c
                 LEFT JOIN messages m ON m.session_id = c.session_id
                 WHERE c.message_count > 0
                   AND COALESCE(c.source, 'claude_code') = 'claude_code'
                 GROUP BY c.session_id
                 HAVING COUNT(m.message_uuid) = 0",
            )?;
            let rows: rusqlite::Result<Vec<_>> = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect();
            rows?
        };

        if orphans.is_empty() {
            return Ok(0);
        }

        for (session_id, conv_file) in &orphans {
            self.log(&format!("  Repairing orphan session {}", session_id));
            if let Some(path) = conv_file {
                tx.execute(
                    "DELETE FROM claude_code_sync_state WHERE file_path = ?",
                    [path],
                )?;
            }
            tx.execute(
                "DELETE FROM conversations WHERE session_id = ?",
                [session_id],
            )?;
        }

        tx.commit()?;
        Ok(orphans.len())
    }

    /// Index all conversations from the last N days.
    #[allow(dead_code)]
    pub fn index_all(&mut self, days_back: Option<i64>) -> Result<()> {
        let files = self.scan_conversations(days_back);
        self.log(&format!(
            "Found {} conversation files to index",
            files.len()
        ));

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
    fn test_parse_conversation_file_logs_malformed_json_lines() {
        let file = write_temp_jsonl(&[
            r#"{"uuid":"uuid1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess1","message":{"role":"user","content":"Hello world"}}"#,
            r#"{"uuid":"broken""#,
            r#"{"uuid":"uuid2","parentUuid":"uuid1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sess1","message":{"role":"assistant","content":"Hi"}}"#,
        ]);
        let logs = std::cell::RefCell::new(Vec::new());
        let log = |msg: &str| logs.borrow_mut().push(msg.to_string());

        let (_, messages, skipped_lines) =
            ConversationIndexer::parse_conversation_file_with_logger(file.path(), Some(&log))
                .unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(skipped_lines, 1);
        let logs = logs.borrow();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("Error parsing line 2"));
        assert!(logs[0].contains(file.path().to_string_lossy().as_ref()));
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
        assert!(
            meta_uuids.contains("a1"),
            "assistant message should be meta"
        );
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

    /// Write a JSONL file inside `dir` and return its path. Unlike
    /// `write_temp_jsonl` this keeps the file in a real (named) directory so
    /// the indexer's project_path derivation has something to work with.
    fn write_jsonl_in_dir(dir: &std::path::Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.join(format!("{}.jsonl", name));
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f.flush().unwrap();
        f.sync_all().unwrap();
        path
    }

    /// Phase 2-2 / 2-3 regression: Claude Code resume sessions re-emit parent
    /// messages with the same `message_uuid` under a new `session_id`. Without
    /// `INSERT OR IGNORE` the second indexing fails on PRIMARY KEY collision
    /// and (pre-fix) leaves an orphan conversations row. With the fix:
    ///   - both conversations rows exist
    ///   - duplicated messages stay attributed to the first session
    ///   - `message_count` reflects the actual COUNT(*) per session, not the
    ///     parsed JSONL length
    ///   - search content (FTS) for the new messages is reachable
    #[test]
    fn test_resume_session_with_shared_uuids() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let mut indexer = ConversationIndexer::new(&db_path, true).unwrap();

        let proj = dir.path();

        // Session A: 3 messages.
        let file_a = write_jsonl_in_dir(
            proj,
            "sessA",
            &[
                r#"{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sessA","message":{"role":"user","content":"Initial question about pdf rendering"}}"#,
                r#"{"uuid":"u2","parentUuid":"u1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sessA","message":{"role":"assistant","content":"Reply about pdf"}}"#,
                r#"{"uuid":"u3","parentUuid":"u2","isSidechain":false,"timestamp":"2025-01-15T10:02:00Z","type":"user","sessionId":"sessA","message":{"role":"user","content":"Follow-up in A"}}"#,
            ],
        );
        indexer.index_conversation(&file_a).unwrap();

        // Session B: a resume of A. It re-emits u1, u2, u3 (same UUIDs) and
        // appends two new messages u4, u5. The new messages contain the
        // distinctive token "devbox-needle" so we can verify they're FTS-able.
        let file_b = write_jsonl_in_dir(
            proj,
            "sessB",
            &[
                r#"{"uuid":"u1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sessB","message":{"role":"user","content":"Initial question about pdf rendering"}}"#,
                r#"{"uuid":"u2","parentUuid":"u1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sessB","message":{"role":"assistant","content":"Reply about pdf"}}"#,
                r#"{"uuid":"u3","parentUuid":"u2","isSidechain":false,"timestamp":"2025-01-15T10:02:00Z","type":"user","sessionId":"sessB","message":{"role":"user","content":"Follow-up in A"}}"#,
                r#"{"uuid":"u4","parentUuid":"u3","isSidechain":false,"timestamp":"2025-01-15T11:00:00Z","type":"user","sessionId":"sessB","message":{"role":"user","content":"devbox-needle question"}}"#,
                r#"{"uuid":"u5","parentUuid":"u4","isSidechain":false,"timestamp":"2025-01-15T11:01:00Z","type":"assistant","sessionId":"sessB","message":{"role":"assistant","content":"devbox-needle answer"}}"#,
            ],
        );
        indexer.index_conversation(&file_b).unwrap();

        let conn = indexer.connection();

        // Both conversations rows exist.
        let conv_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(conv_count, 2, "expected 2 conversation rows");

        // Duplicate UUIDs stayed attributed to A.
        let msgs_under_a: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'sessA'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let msgs_under_b: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'sessB'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(msgs_under_a, 3, "session A keeps its 3 original messages");
        assert_eq!(
            msgs_under_b, 2,
            "session B only owns the new u4, u5 after OR IGNORE"
        );

        // message_count reflects COUNT(*), not parsed JSONL length.
        let conv_a_count: i64 = conn
            .query_row(
                "SELECT message_count FROM conversations WHERE session_id = 'sessA'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let conv_b_count: i64 = conn
            .query_row(
                "SELECT message_count FROM conversations WHERE session_id = 'sessB'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(conv_a_count, 3);
        assert_eq!(
            conv_b_count, 2,
            "must NOT be 5 — INSERT OR IGNORE skipped u1-u3"
        );

        // FTS finds the new devbox-needle content.
        let fts_hit: i64 = conn
            .query_row(
                // Wrap in double-quotes because FTS5 treats `-` as the column
                // negation operator otherwise.
                r#"SELECT COUNT(*) FROM message_content_fts WHERE message_content_fts MATCH '"devbox-needle"'"#,
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            fts_hit >= 2,
            "FTS should find both u4 and u5 (got {})",
            fts_hit
        );

        // Both files have a sync_state entry.
        let sync_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM claude_code_sync_state", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(sync_count, 2);
    }

    /// Phase 2-1 regression: after a successful index, the conversations row,
    /// the messages rows, and the sync_state entry are all in place — and
    /// `conversations.message_count` equals the actual COUNT(*).
    /// This is the invariant that guarantees content search can find the
    /// session; it's broken when transaction atomicity is broken.
    #[test]
    fn test_index_creates_consistent_state() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let mut indexer = ConversationIndexer::new(&db_path, true).unwrap();

        let file = write_jsonl_in_dir(
            dir.path(),
            "session1",
            &[
                r#"{"uuid":"x1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess1","message":{"role":"user","content":"hello"}}"#,
                r#"{"uuid":"x2","parentUuid":"x1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sess1","message":{"role":"assistant","content":"world"}}"#,
            ],
        );
        indexer.index_conversation(&file).unwrap();

        let conn = indexer.connection();

        // No orphans: every conv row's message_count matches its COUNT(*).
        let inconsistent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (
                    SELECT c.session_id
                    FROM conversations c
                    LEFT JOIN messages m ON m.session_id = c.session_id
                    GROUP BY c.session_id
                    HAVING c.message_count != COUNT(m.message_uuid)
                 )",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            inconsistent, 0,
            "post-index DB must have no orphan/skewed rows"
        );

        // sync_state was written inside the same transaction.
        let sync_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claude_code_sync_state WHERE file_path = ?",
                [file.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sync_count, 1, "sync_state must be recorded on success");

        let conv_count: i64 = conn
            .query_row(
                "SELECT message_count FROM conversations WHERE session_id = 'sess1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(conv_count, 2);
    }

    /// Reverse-order indexing must NOT leave an empty `conversations` row
    /// behind. When session B (a resume of A) is indexed *before* A, all of
    /// B's UUIDs land in `messages` first. Then when A is indexed, its
    /// JSONL re-emits those same UUIDs, INSERT OR IGNORE skips them all,
    /// and `actual_count` is 0. We must drop the fresh conv row instead of
    /// keeping a ghost that's invisible to content search but shows up in
    /// `list`. The content for A is still searchable — it lives under B's
    /// session_id, which is the correct behavior given immutable messages.
    #[test]
    fn test_reverse_order_resume_drops_empty_conv_row() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let mut indexer = ConversationIndexer::new(&db_path, true).unwrap();

        // B (the resume) — index it FIRST.
        let file_b = write_jsonl_in_dir(
            dir.path(),
            "sessB_rev",
            &[
                r#"{"uuid":"r1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sessB_rev","message":{"role":"user","content":"shared content"}}"#,
                r#"{"uuid":"r2","parentUuid":"r1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sessB_rev","message":{"role":"assistant","content":"shared reply"}}"#,
                r#"{"uuid":"r3","parentUuid":"r2","isSidechain":false,"timestamp":"2025-01-15T10:02:00Z","type":"user","sessionId":"sessB_rev","message":{"role":"user","content":"shared followup"}}"#,
            ],
        );
        indexer.index_conversation(&file_b).unwrap();

        // A (the original) — same UUIDs, but indexed SECOND.
        let file_a = write_jsonl_in_dir(
            dir.path(),
            "sessA_rev",
            &[
                r#"{"uuid":"r1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sessA_rev","message":{"role":"user","content":"shared content"}}"#,
                r#"{"uuid":"r2","parentUuid":"r1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sessA_rev","message":{"role":"assistant","content":"shared reply"}}"#,
                r#"{"uuid":"r3","parentUuid":"r2","isSidechain":false,"timestamp":"2025-01-15T10:02:00Z","type":"user","sessionId":"sessA_rev","message":{"role":"user","content":"shared followup"}}"#,
            ],
        );
        indexer.index_conversation(&file_a).unwrap();

        let conn = indexer.connection();

        // Only B's conv row should exist — A's was dropped because actual_count == 0.
        let conv_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(conv_count, 1, "ghost A conv row must NOT remain");

        let sessa_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE session_id = 'sessA_rev'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sessa_exists, 0);

        let sessb_count: i64 = conn
            .query_row(
                "SELECT message_count FROM conversations WHERE session_id = 'sessB_rev'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sessb_count, 3);

        // sync_state for A's file IS recorded so we don't re-parse it forever.
        let sync_a: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claude_code_sync_state WHERE file_path = ?",
                [file_a.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sync_a, 1,
            "sync_state must be written even when conv row is dropped"
        );

        // Invariant: no orphan / no skewed rows.
        let inconsistent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (
                    SELECT c.session_id FROM conversations c
                    LEFT JOIN messages m ON m.session_id = c.session_id
                    GROUP BY c.session_id
                    HAVING c.message_count != COUNT(m.message_uuid)
                 )",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inconsistent, 0);
    }

    /// Re-indexing the same file with the same mtime must be a no-op:
    /// the mtime check at the top of `index_conversation` should short-circuit
    /// before opening any transaction. Regression guard for the sync_state
    /// fast-path.
    #[test]
    fn test_reindex_same_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let mut indexer = ConversationIndexer::new(&db_path, true).unwrap();

        let file = write_jsonl_in_dir(
            dir.path(),
            "session1",
            &[
                r#"{"uuid":"y1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess1","message":{"role":"user","content":"hi"}}"#,
                r#"{"uuid":"y2","parentUuid":"y1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sess1","message":{"role":"assistant","content":"hello"}}"#,
            ],
        );

        indexer.index_conversation(&file).unwrap();
        // Snapshot indexed_at to detect whether the second call updated the row.
        let first_indexed_at: String = indexer
            .connection()
            .query_row(
                "SELECT indexed_at FROM conversations WHERE session_id = 'sess1'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // SQLite CURRENT_TIMESTAMP has 1-second resolution; sleep enough to
        // make any UPDATE observable.
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Second index of the same file: mtime unchanged, must be a no-op.
        indexer.index_conversation(&file).unwrap();

        let conn = indexer.connection();
        let msg_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'sess1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(msg_count, 2, "must not duplicate messages on re-index");

        let second_indexed_at: String = conn
            .query_row(
                "SELECT indexed_at FROM conversations WHERE session_id = 'sess1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            first_indexed_at, second_indexed_at,
            "no-op re-index must NOT bump indexed_at (mtime fast-path)"
        );
    }

    /// UPDATE path: existing session, file grew with new messages on disk.
    /// Verifies the "incremental append" branch (existing conv row found,
    /// new messages diffed via existing_uuids set) and that message_count
    /// reflects the new total via COUNT(*).
    #[test]
    fn test_reindex_file_grew_updates_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let mut indexer = ConversationIndexer::new(&db_path, true).unwrap();

        let path = dir.path().join("grow.jsonl");

        // First write: 2 messages.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, r#"{{"uuid":"g1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"grow","message":{{"role":"user","content":"q1"}}}}"#).unwrap();
            writeln!(f, r#"{{"uuid":"g2","parentUuid":"g1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"grow","message":{{"role":"assistant","content":"a1"}}}}"#).unwrap();
            f.sync_all().unwrap();
        }
        indexer.index_conversation(&path).unwrap();

        // Wait a beat then append 2 more messages and bump mtime.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, r#"{{"uuid":"g3","parentUuid":"g2","isSidechain":false,"timestamp":"2025-01-15T10:02:00Z","type":"user","sessionId":"grow","message":{{"role":"user","content":"q2"}}}}"#).unwrap();
            writeln!(f, r#"{{"uuid":"g4","parentUuid":"g3","isSidechain":false,"timestamp":"2025-01-15T10:03:00Z","type":"assistant","sessionId":"grow","message":{{"role":"assistant","content":"a2"}}}}"#).unwrap();
            f.sync_all().unwrap();
        }

        indexer.index_conversation(&path).unwrap();

        let conn = indexer.connection();
        let msg_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'grow'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            msg_count, 4,
            "all 4 messages must be present after grow + reindex"
        );

        let conv_count: i64 = conn
            .query_row(
                "SELECT message_count FROM conversations WHERE session_id = 'grow'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            conv_count, 4,
            "message_count must match COUNT(*) post-update"
        );

        // Invariant: no orphans, no skew.
        let inconsistent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (
                    SELECT c.session_id FROM conversations c
                    LEFT JOIN messages m ON m.session_id = c.session_id
                    GROUP BY c.session_id
                    HAVING c.message_count != COUNT(m.message_uuid)
                 )",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inconsistent, 0);
    }

    /// Phase 2-4 regression: `repair_orphan_conversations` cleans up rows left
    /// behind by the pre-0.12.1 bug (conversations row with message_count > 0
    /// but no messages) and removes the matching sync_state entry so the file
    /// will be re-parsed.
    #[test]
    fn test_repair_orphan_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let mut indexer = ConversationIndexer::new(&db_path, true).unwrap();

        // Index one healthy session so we have a non-orphan to leave alone.
        let healthy = write_jsonl_in_dir(
            dir.path(),
            "healthy",
            &[
                r#"{"uuid":"h1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"healthy","message":{"role":"user","content":"keep me"}}"#,
            ],
        );
        indexer.index_conversation(&healthy).unwrap();

        // Manually inject orphans that mimic the pre-fix bug.
        let orphan_file = dir
            .path()
            .join("orphan1.jsonl")
            .to_string_lossy()
            .to_string();
        let conn = indexer.connection();
        conn.execute(
            "INSERT INTO conversations (session_id, project_path, conversation_file, conversation_summary, first_message_at, last_message_at, message_count) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params!["orphan1", "/p", &orphan_file, "ghost", "2025-01-01T00:00:00Z", "2025-01-01T00:00:00Z", 42_i64],
        ).unwrap();
        conn.execute(
            "INSERT INTO claude_code_sync_state (file_path, mtime) VALUES (?, ?)",
            rusqlite::params![&orphan_file, 1234567890.0_f64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations (session_id, project_path, conversation_file, conversation_summary, first_message_at, last_message_at, message_count) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params!["orphan2", "/p", "/tmp/nofile.jsonl", "ghost2", "2025-01-01T00:00:00Z", "2025-01-01T00:00:00Z", 7_i64],
        ).unwrap();

        // Sanity: 3 conversations now (1 healthy + 2 orphans).
        let pre: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pre, 3);

        let repaired = indexer.repair_orphan_conversations().unwrap();
        assert_eq!(repaired, 2);

        let conn = indexer.connection();
        let post: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(post, 1, "only the healthy row should remain");

        // sync_state for the orphan file should be gone (so re-index will re-parse).
        let sync_orphan: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claude_code_sync_state WHERE file_path = ?",
                [&orphan_file],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sync_orphan, 0);

        // Calling repair again is a no-op.
        assert_eq!(indexer.repair_orphan_conversations().unwrap(), 0);
    }
}
