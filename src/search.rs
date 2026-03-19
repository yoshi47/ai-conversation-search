use std::collections::HashMap;

use chrono::{DateTime, Local, TimeDelta, Utc};
use rusqlite::Connection;
use serde::Serialize;

use crate::date_utils::build_date_filter;
use crate::db;
use crate::error::{AppError, Result};

/// Common filter parameters for search and list operations.
pub struct SearchFilter<'a> {
    pub days_back: Option<i64>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub date: Option<&'a str>,
    pub limit: i64,
    pub project_path: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub source: Option<&'a str>,
}

/// A single search result row (messages JOIN conversations).
#[derive(Debug, Clone, Serialize)]
pub struct SearchResultRow {
    pub message_uuid: String,
    pub session_id: String,
    pub parent_uuid: Option<String>,
    pub timestamp: String,
    pub message_type: String,
    pub project_path: Option<String>,
    pub depth: i64,
    pub is_sidechain: bool,
    pub context_snippet: String,
    pub conversation_summary: Option<String>,
    pub conversation_file: Option<String>,
    pub source: Option<String>,
}

impl SearchResultRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            message_uuid: row.get("message_uuid")?,
            session_id: row.get("session_id")?,
            parent_uuid: row.get("parent_uuid")?,
            timestamp: row.get("timestamp")?,
            message_type: row.get("message_type")?,
            project_path: row.get("project_path")?,
            depth: row.get("depth")?,
            is_sidechain: row.get("is_sidechain")?,
            context_snippet: row.get("context_snippet")?,
            conversation_summary: row.get("conversation_summary")?,
            conversation_file: row.get("conversation_file")?,
            source: row.get("source")?,
        })
    }
}

/// A row from the conversations table.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationRow {
    pub session_id: String,
    pub project_path: Option<String>,
    pub repo_root: Option<String>,
    pub conversation_file: Option<String>,
    pub root_message_uuid: Option<String>,
    pub leaf_message_uuid: Option<String>,
    pub conversation_summary: Option<String>,
    pub first_message_at: Option<String>,
    pub last_message_at: Option<String>,
    pub message_count: i64,
    pub source: Option<String>,
    pub indexed_at: Option<String>,
}

impl ConversationRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            session_id: row.get("session_id")?,
            project_path: row.get("project_path")?,
            repo_root: row.get("repo_root")?,
            conversation_file: row.get("conversation_file")?,
            root_message_uuid: row.get("root_message_uuid")?,
            leaf_message_uuid: row.get("leaf_message_uuid")?,
            conversation_summary: row.get("conversation_summary")?,
            first_message_at: row.get("first_message_at")?,
            last_message_at: row.get("last_message_at")?,
            message_count: row.get("message_count")?,
            source: row.get("source")?,
            indexed_at: row.get("indexed_at")?,
        })
    }
}

/// A row from the messages table.
#[derive(Debug, Clone, Serialize)]
pub struct MessageRow {
    pub message_uuid: String,
    pub session_id: String,
    pub parent_uuid: Option<String>,
    pub is_sidechain: bool,
    pub depth: i64,
    pub timestamp: String,
    pub message_type: String,
    pub project_path: Option<String>,
    pub conversation_file: Option<String>,
    pub summary: Option<String>,
    pub full_content: String,
    pub is_summarized: bool,
    pub is_tool_noise: bool,
    pub is_meta_conversation: bool,
}

impl MessageRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            message_uuid: row.get("message_uuid")?,
            session_id: row.get("session_id")?,
            parent_uuid: row.get("parent_uuid")?,
            is_sidechain: row.get("is_sidechain")?,
            depth: row.get("depth")?,
            timestamp: row.get("timestamp")?,
            message_type: row.get("message_type")?,
            project_path: row.get("project_path")?,
            conversation_file: row.get("conversation_file")?,
            summary: row.get("summary")?,
            full_content: row.get("full_content")?,
            is_summarized: row.get("is_summarized")?,
            is_tool_noise: row.get("is_tool_noise")?,
            is_meta_conversation: row.get("is_meta_conversation")?,
        })
    }
}

/// Conversation context result.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationContext {
    pub message: Option<MessageRow>,
    pub ancestors: Vec<MessageRow>,
    pub children: Vec<MessageRow>,
    pub conversation: Option<ConversationRow>,
    pub context_depth: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Conversation tree result.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationTree {
    pub conversation: Option<ConversationRow>,
    pub tree: Vec<TreeNode>,
    pub total_messages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A node in the conversation tree.
#[derive(Debug, Clone, Serialize)]
pub struct TreeNode {
    pub message_uuid: String,
    pub session_id: String,
    pub parent_uuid: Option<String>,
    pub is_sidechain: bool,
    pub depth: i64,
    pub timestamp: String,
    pub message_type: String,
    pub project_path: Option<String>,
    pub summary: Option<String>,
    pub full_content: String,
    pub children: Vec<TreeNode>,
}

/// Escape special LIKE characters.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Extract a snippet around the first occurrence of any search term in content.
/// Returns a window of `max_len` chars centered on the match, with `**` highlighting.
fn extract_snippet(content: &str, search_terms: &[&str], max_len: usize) -> String {
    let content_lower = content.to_lowercase();

    // Find the earliest match position
    let mut best_pos: Option<(usize, usize)> = None; // (byte_start, byte_len)
    for term in search_terms {
        let term_lower = term.to_lowercase();
        if let Some(byte_idx) = content_lower.find(&term_lower) {
            match best_pos {
                None => best_pos = Some((byte_idx, term.len())),
                Some((prev, _)) if byte_idx < prev => best_pos = Some((byte_idx, term.len())),
                _ => {}
            }
        }
    }

    let Some((match_byte_start, _)) = best_pos else {
        // No match found, return beginning of content
        return content.chars().take(max_len).collect();
    };

    // Convert byte position to char position
    let match_char_start = content[..match_byte_start].chars().count();

    // Calculate window: center on match
    let half_window = max_len / 2;
    let window_start = match_char_start.saturating_sub(half_window);
    let snippet: String = content.chars().skip(window_start).take(max_len).collect();

    // Add highlight markers for all terms
    let mut result = snippet;
    for term in search_terms {
        let term_lower = term.to_lowercase();
        let mut highlighted = String::new();
        let mut remaining = result.as_str();
        while !remaining.is_empty() {
            let remaining_lower = remaining.to_lowercase();
            if let Some(idx) = remaining_lower.find(&term_lower) {
                highlighted.push_str(&remaining[..idx]);
                let matched = &remaining[idx..idx + term_lower.len()];
                highlighted.push_str("**");
                highlighted.push_str(matched);
                highlighted.push_str("**");
                remaining = &remaining[idx + term_lower.len()..];
            } else {
                highlighted.push_str(remaining);
                break;
            }
        }
        result = highlighted;
    }

    let prefix = if window_start > 0 { "..." } else { "" };
    let suffix = if window_start + max_len < content.chars().count() { "..." } else { "" };
    format!("{}{}{}", prefix, result, suffix)
}

/// Convert UTC ISO timestamp to local time for display.
pub fn format_timestamp(iso_timestamp: &str, include_date: bool, include_seconds: bool) -> String {
    let cleaned = iso_timestamp.replace('Z', "+00:00");
    let dt_utc = match DateTime::parse_from_rfc3339(&cleaned) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(_) => {
            // Try parsing without timezone
            if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(iso_timestamp, "%Y-%m-%dT%H:%M:%S%.f") {
                naive.and_utc()
            } else if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(iso_timestamp, "%Y-%m-%dT%H:%M:%S") {
                naive.and_utc()
            } else {
                return iso_timestamp.to_string();
            }
        }
    };

    let dt_local: DateTime<Local> = dt_utc.into();

    match (include_date, include_seconds) {
        (true, true) => dt_local.format("%Y-%m-%d %H:%M:%S").to_string(),
        (true, false) => dt_local.format("%Y-%m-%d %H:%M").to_string(),
        (false, true) => dt_local.format("%H:%M:%S").to_string(),
        (false, false) => dt_local.format("%H:%M").to_string(),
    }
}

pub struct ConversationSearch {
    conn: Connection,
    db_path: String,
    fts_rebuilt: bool,
}

impl ConversationSearch {
    pub fn new(db_path: &str) -> Result<Self> {
        let resolved = db::expand_path(db_path);
        if !resolved.exists() {
            return Err(AppError::General(format!(
                "Database not found at {}. Run 'ai-conversation-search init' first.",
                resolved.display()
            )));
        }

        let conn = db::connect(db_path, true)?;
        Ok(Self {
            conn,
            db_path: db_path.to_string(),
            fts_rebuilt: false,
        })
    }

    pub fn search_conversations(
        &mut self,
        query: &str,
        filter: &SearchFilter<'_>,
    ) -> Result<Vec<SearchResultRow>> {
        let days_back = filter.days_back;
        let since = filter.since;
        let until = filter.until;
        let date = filter.date;
        let limit = filter.limit;
        let project_path = filter.project_path;
        let repo = filter.repo;
        let source = filter.source;

        if days_back.is_some() && (since.is_some() || until.is_some() || date.is_some()) {
            return Err(AppError::General(
                "Cannot use --days with --since/--until/--date".to_string(),
            ));
        }

        let trimmed = query.trim();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        // Check if any search term has < 3 characters (trigram minimum)
        let terms: Vec<&str> = trimmed.split_whitespace().collect();
        let has_short_term = terms.iter().any(|t| t.chars().count() < 3);

        let sql = if trimmed.is_empty() {
            let mut sql = String::from(
                "SELECT m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, m.message_type, m.project_path, m.depth, m.is_sidechain, SUBSTR(m.full_content, 1, 500) as context_snippet, c.conversation_summary, c.conversation_file, c.source FROM messages m JOIN conversations c ON m.session_id = c.session_id WHERE m.is_meta_conversation = FALSE"
            );

            Self::append_filters(&mut sql, &mut params, filter)?;

            sql.push_str(" ORDER BY m.timestamp DESC LIMIT ?");
            params.push(Box::new(limit));
            sql
        } else if has_short_term {
            // Trigram requires >= 3 characters per term; fall back to LIKE for short terms
            let mut sql = String::from(
                "SELECT m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, m.message_type, m.project_path, m.depth, m.is_sidechain, SUBSTR(m.full_content, 1, 500) as context_snippet, c.conversation_summary, c.conversation_file, c.source FROM messages m JOIN conversations c ON m.session_id = c.session_id WHERE m.is_meta_conversation = FALSE"
            );

            for term in &terms {
                sql.push_str(" AND m.full_content LIKE ? ESCAPE '\\'");
                params.push(Box::new(format!("%{}%", escape_like(term))));
            }

            Self::append_filters(&mut sql, &mut params, filter)?;

            sql.push_str(" ORDER BY m.timestamp DESC LIMIT ?");
            params.push(Box::new(limit));
            sql
        } else {
            // Sanitize query for FTS5 trigram tokenizer
            let fts_query = if !query.contains(" AND ")
                && !query.contains(" OR ")
                && !query.contains(" NOT ")
                && !query.contains('"')
            {
                let terms: Vec<&str> = query.split_whitespace().collect();
                if terms.len() == 1 {
                    format!("\"{}\"", terms[0])
                } else {
                    terms.iter().map(|t| format!("\"{}\"", t)).collect::<Vec<_>>().join(" AND ")
                }
            } else {
                query.to_string()
            };

            // Two-phase query to work around SQLite trigram FTS performance issue.
            // SQLite's planner incorrectly uses idx_is_meta_conversation as the driving index,
            // scanning ~all messages and checking trigram FTS for each row (O(N) full table scan).
            // Phase 1: Get matching rowids from FTS (fast, ~50ms for 1700 matches).
            // Phase 2: Query messages+conversations by rowid IN batches.
            let rowids = self.query_fts_rowids(&fts_query)?;

            if rowids.is_empty() {
                return Ok(Vec::new());
            }

            // Process in batches to stay within SQLITE_MAX_VARIABLE_NUMBER
            const BATCH_SIZE: usize = 500;
            let mut all_results: Vec<SearchResultRow> = Vec::new();

            for chunk in rowids.chunks(BATCH_SIZE) {
                let mut batch_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

                let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let mut sql = format!(
                    "SELECT m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, m.message_type, m.project_path, m.depth, m.is_sidechain, m.full_content as context_snippet, c.conversation_summary, c.conversation_file, c.source FROM messages m JOIN conversations c ON m.session_id = c.session_id WHERE m.rowid IN ({}) AND m.is_meta_conversation = FALSE",
                    placeholders
                );

                for rowid in chunk {
                    batch_params.push(Box::new(*rowid));
                }

                Self::append_filters(&mut sql, &mut batch_params, filter)?;
                sql.push_str(" ORDER BY m.timestamp DESC");

                let batch_results = self.execute_search_typed(&sql, &batch_params)?;
                all_results.extend(batch_results);
            }

            // Sort all results by timestamp DESC and take limit
            all_results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
            all_results.truncate(limit as usize);

            // Post-process: extract snippets with highlighting around match locations
            let search_terms: Vec<&str> = trimmed.split_whitespace().collect();
            for row in &mut all_results {
                row.context_snippet = extract_snippet(&row.context_snippet, &search_terms, 200);
            }
            return Ok(all_results);
        };

        self.execute_search_typed(&sql, &params)
    }

    fn append_filters(
        sql: &mut String,
        params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
        filter: &SearchFilter<'_>,
    ) -> Result<()> {
        let days_back = filter.days_back;
        let since = filter.since;
        let until = filter.until;
        let date = filter.date;
        let project_path = filter.project_path;
        let repo = filter.repo;
        let source = filter.source;
        if date.is_some() || since.is_some() || until.is_some() {
            let (date_sql, date_params) = build_date_filter(since, until, date)?;
            if !date_sql.is_empty() {
                sql.push_str(&format!(" AND m.{}", date_sql));
                for p in date_params {
                    params.push(Box::new(p));
                }
            }
        } else if let Some(d) = days_back {
            let cutoff = (Local::now() - TimeDelta::days(d)).naive_local();
            sql.push_str(" AND m.timestamp >= ?");
            params.push(Box::new(cutoff.format("%Y-%m-%dT%H:%M:%S").to_string()));
        }

        if let Some(pp) = project_path {
            sql.push_str(" AND m.project_path = ?");
            params.push(Box::new(pp.to_string()));
        }

        if let Some(r) = repo {
            sql.push_str(" AND c.repo_root LIKE ? ESCAPE '\\'");
            params.push(Box::new(format!("%{}%", escape_like(r))));
        }

        if let Some(s) = source {
            sql.push_str(" AND c.source = ?");
            params.push(Box::new(s.to_string()));
        }

        Ok(())
    }

    fn query_fts_rowids(&self, fts_query: &str) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT rowid FROM message_content_fts WHERE full_content MATCH ?"
        )?;
        let rowids = stmt
            .query_map(rusqlite::params![fts_query], |row| row.get(0))?
            .collect::<std::result::Result<Vec<i64>, _>>()?;
        Ok(rowids)
    }

    fn execute_search_typed(
        &mut self,
        sql: &str,
        params: &[Box<dyn rusqlite::types::ToSql>],
    ) -> Result<Vec<SearchResultRow>> {
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        match self.query_rows(sql, &param_refs, SearchResultRow::from_row) {
            Ok(results) => Ok(results),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("fts5: missing row") && !self.fts_rebuilt {
                    eprintln!("FTS index corruption detected, rebuilding...");
                    self.rebuild_fts()?;
                    eprintln!("FTS index rebuilt, retrying search...");
                    self.query_rows(sql, &param_refs, SearchResultRow::from_row)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn query_rows<T, F>(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::types::ToSql],
        map_fn: F,
    ) -> Result<Vec<T>>
    where
        F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| map_fn(row))?;
        let mut results = Vec::new();
        for row in rows {
            match row {
                Ok(r) => results.push(r),
                Err(e) => log::warn!("Error reading row: {}", e),
            }
        }
        Ok(results)
    }

    pub fn get_conversation_context(
        &self,
        message_uuid: &str,
        depth: i32,
    ) -> Result<ConversationContext> {
        // Get target message
        let target = self.query_rows(
            "SELECT * FROM messages WHERE message_uuid = ?",
            &[&message_uuid as &dyn rusqlite::types::ToSql],
            MessageRow::from_row,
        )?;

        if target.is_empty() {
            return Ok(ConversationContext {
                message: None,
                ancestors: Vec::new(),
                children: Vec::new(),
                conversation: None,
                context_depth: 0,
                error: Some(format!("Message {} not found", message_uuid)),
            });
        }

        let target_msg = &target[0];

        // Walk up ancestors
        let mut ancestors = Vec::new();
        let mut current_uuid = target_msg.parent_uuid.clone();
        let mut levels = 0;

        while let Some(ref uuid) = current_uuid {
            if levels >= depth {
                break;
            }
            let parent = self.query_rows(
                "SELECT * FROM messages WHERE message_uuid = ?",
                &[uuid as &dyn rusqlite::types::ToSql],
                MessageRow::from_row,
            )?;
            let Some(parent_msg) = parent.into_iter().next() else {
                break;
            };
            current_uuid = parent_msg.parent_uuid.clone();
            ancestors.insert(0, parent_msg);
            levels += 1;
        }

        // Get conversation metadata
        let session_id = &target_msg.session_id;
        let conv = self.query_rows(
            "SELECT * FROM conversations WHERE session_id = ?",
            &[session_id as &dyn rusqlite::types::ToSql],
            ConversationRow::from_row,
        )?;

        Ok(ConversationContext {
            message: Some(target_msg.clone()),
            ancestors,
            children: Vec::new(),
            conversation: conv.into_iter().next(),
            context_depth: levels,
            error: None,
        })
    }

    pub fn get_conversation_tree(
        &self,
        session_id: &str,
    ) -> Result<ConversationTree> {
        let messages = self.query_rows(
            "SELECT * FROM messages WHERE session_id = ? ORDER BY timestamp ASC",
            &[&session_id as &dyn rusqlite::types::ToSql],
            MessageRow::from_row,
        )?;

        let conv = self.query_rows(
            "SELECT * FROM conversations WHERE session_id = ?",
            &[&session_id as &dyn rusqlite::types::ToSql],
            ConversationRow::from_row,
        )?;

        if conv.is_empty() {
            return Ok(ConversationTree {
                conversation: None,
                tree: Vec::new(),
                total_messages: 0,
                error: Some(format!("Conversation {} not found", session_id)),
            });
        }

        let tree = Self::build_tree(&messages);

        Ok(ConversationTree {
            conversation: conv.into_iter().next(),
            tree,
            total_messages: messages.len(),
            error: None,
        })
    }

    fn build_tree(messages: &[MessageRow]) -> Vec<TreeNode> {
        let mut msg_map: HashMap<String, &MessageRow> = HashMap::new();
        for msg in messages {
            msg_map.insert(msg.message_uuid.clone(), msg);
        }

        let mut roots = Vec::new();
        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();

        for msg in messages {
            if let Some(ref parent_uuid) = msg.parent_uuid {
                if msg_map.contains_key(parent_uuid) {
                    children_map.entry(parent_uuid.clone()).or_default().push(msg.message_uuid.clone());
                    continue;
                }
            }
            roots.push(msg.message_uuid.clone());
        }

        fn build_node(
            uuid: &str,
            msg_map: &HashMap<String, &MessageRow>,
            children_map: &HashMap<String, Vec<String>>,
        ) -> TreeNode {
            let msg = msg_map.get(uuid).expect("build_node called with uuid not in msg_map");
            let children = if let Some(kids) = children_map.get(uuid) {
                kids.iter().map(|k| build_node(k, msg_map, children_map)).collect()
            } else {
                Vec::new()
            };
            TreeNode {
                message_uuid: msg.message_uuid.clone(),
                session_id: msg.session_id.clone(),
                parent_uuid: msg.parent_uuid.clone(),
                is_sidechain: msg.is_sidechain,
                depth: msg.depth,
                timestamp: msg.timestamp.clone(),
                message_type: msg.message_type.clone(),
                project_path: msg.project_path.clone(),
                summary: msg.summary.clone(),
                full_content: msg.full_content.clone(),
                children,
            }
        }

        roots.iter().map(|uuid| build_node(uuid, &msg_map, &children_map)).collect()
    }

    pub fn list_recent_conversations(
        &self,
        filter: &SearchFilter<'_>,
    ) -> Result<Vec<ConversationRow>> {
        let days_back = filter.days_back;
        let since = filter.since;
        let until = filter.until;
        let date = filter.date;
        let limit = filter.limit;
        let project_path = filter.project_path;
        let repo = filter.repo;
        let source = filter.source;

        let effective_days = if days_back.is_none() && since.is_none() && until.is_none() && date.is_none() {
            Some(7i64)
        } else {
            days_back
        };

        if effective_days.is_some() && (since.is_some() || until.is_some() || date.is_some()) {
            return Err(AppError::General(
                "Cannot use --days with --since/--until/--date".to_string(),
            ));
        }

        let mut sql = String::from("SELECT * FROM conversations WHERE 1=1");
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if date.is_some() || since.is_some() || until.is_some() {
            let (date_sql, date_params) = build_date_filter(since, until, date)?;
            if !date_sql.is_empty() {
                let replaced = date_sql.replace("timestamp", "last_message_at");
                sql.push_str(&format!(" AND {}", replaced));
                for p in date_params {
                    params.push(Box::new(p));
                }
            }
        } else if let Some(d) = effective_days {
            let cutoff = (Local::now() - TimeDelta::days(d)).naive_local();
            sql.push_str(" AND last_message_at >= ?");
            params.push(Box::new(cutoff.format("%Y-%m-%dT%H:%M:%S").to_string()));
        }

        if let Some(pp) = project_path {
            sql.push_str(" AND project_path = ?");
            params.push(Box::new(pp.to_string()));
        }

        if let Some(r) = repo {
            sql.push_str(" AND repo_root LIKE ? ESCAPE '\\'");
            params.push(Box::new(format!("%{}%", escape_like(r))));
        }

        if let Some(s) = source {
            sql.push_str(" AND source = ?");
            params.push(Box::new(s.to_string()));
        }

        sql.push_str(" ORDER BY last_message_at DESC LIMIT ?");
        params.push(Box::new(limit));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        self.query_rows(&sql, &param_refs, ConversationRow::from_row)
    }

    pub fn get_full_message_content(&self, message_uuid: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT full_content FROM messages WHERE message_uuid = ?",
                [message_uuid],
                |row| row.get(0),
            )
            .ok()
    }

    #[allow(dead_code)]
    pub fn load_context(
        &self,
        days_back: i64,
        project_path: Option<&str>,
        repo: Option<&str>,
        max_conversations: i64,
        max_messages_per_conv: i64,
    ) -> Result<String> {
        let cutoff = (Local::now() - TimeDelta::days(days_back)).naive_local();
        let cutoff_str = cutoff.format("%Y-%m-%dT%H:%M:%S").to_string();

        let mut sql = String::from(
            "SELECT session_id, conversation_summary, project_path, message_count, last_message_at FROM conversations WHERE last_message_at >= ? AND conversation_summary IS NOT NULL AND conversation_summary != 'None' AND message_count > 2 AND NOT (project_path LIKE '%claude/finder' AND message_count < 5)"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params.push(Box::new(cutoff_str));

        if let Some(pp) = project_path {
            sql.push_str(" AND project_path = ?");
            params.push(Box::new(pp.to_string()));
        }

        if let Some(r) = repo {
            sql.push_str(" AND repo_root LIKE ? ESCAPE '\\'");
            params.push(Box::new(format!("%{}%", escape_like(r))));
        }

        sql.push_str(" ORDER BY last_message_at DESC LIMIT ?");
        params.push(Box::new(max_conversations));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let conversations = self.query_rows(&sql, &param_refs, ConversationRow::from_row)?;

        if conversations.is_empty() {
            return Ok(format!("No conversations found in the last {} day(s).", days_back));
        }

        let day_word = if days_back == 1 { "" } else { "s" };
        let mut lines = vec![format!("# Conversations (last {} day{})\n", days_back, day_word)];

        for conv in &conversations {
            let session_id = &conv.session_id;
            let summary = conv.conversation_summary.as_deref().unwrap_or("");
            let project = conv.project_path.as_deref().unwrap_or("");
            let msg_count = conv.message_count;
            let last_at = conv.last_message_at.as_deref().unwrap_or("");

            let date_str = format_timestamp(last_at, true, false);
            let session_short = &session_id[..std::cmp::min(8, session_id.len())];

            lines.push(format!("## [{}] {}", session_short, summary));
            lines.push(format!("**{} msgs** | {} | {}\n", msg_count, project, date_str));

            // Fetch messages
            let msg_sql = "SELECT message_uuid, timestamp, message_type, summary, is_sidechain, project_path, is_tool_noise FROM messages WHERE session_id = ? AND is_tool_noise = FALSE AND is_meta_conversation = FALSE ORDER BY timestamp DESC LIMIT ?";
            let mut msg_results = self.query_rows(
                msg_sql,
                &[session_id as &dyn rusqlite::types::ToSql, &max_messages_per_conv as &dyn rusqlite::types::ToSql],
                |row| {
                    Ok((
                        row.get::<_, String>("message_uuid")?,
                        row.get::<_, String>("timestamp")?,
                        row.get::<_, String>("message_type")?,
                        row.get::<_, Option<String>>("summary")?,
                        row.get::<_, bool>("is_sidechain")?,
                    ))
                },
            )?;

            msg_results.reverse();

            for (uuid, timestamp, msg_type, summary_opt, is_sidechain) in &msg_results {
                let msg_summary = summary_opt.as_deref().unwrap_or("");
                if msg_summary.is_empty()
                    || msg_summary.starts_with("[Tool")
                    || msg_summary == "[Tool result]"
                    || msg_summary.starts_with("[Request interrupted")
                    || msg_summary.trim().len() < 10
                {
                    continue;
                }

                let msg_time = format_timestamp(timestamp, false, false);
                let icon = if msg_type == "user" { "\u{1f464}" } else { "\u{1f916}" };
                let branch = if *is_sidechain { "\u{1f33f} " } else { "" };
                let uuid_short = &uuid[..8.min(uuid.len())];

                lines.push(format!("{} {} `{}` {}{}", icon, msg_time, uuid_short, branch, msg_summary));
            }

            lines.push(String::new());
        }

        Ok(lines.join("\n"))
    }

    fn rebuild_fts(&mut self) -> Result<()> {
        let rw_conn = crate::db::connect(&self.db_path, false)?;
        rw_conn.execute(
            "INSERT INTO message_content_fts(message_content_fts) VALUES('rebuild')",
            [],
        )?;
        self.fts_rebuilt = true;
        Ok(())
    }
}

#[cfg(test)]
impl ConversationSearch {
    fn from_connection(conn: Connection) -> Self {
        Self {
            conn,
            db_path: String::new(),
            fts_rebuilt: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_filter() -> SearchFilter<'static> {
        SearchFilter {
            days_back: None,
            since: None,
            until: None,
            date: None,
            limit: 10,
            project_path: None,
            repo: None,
            source: None,
        }
    }

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(include_str!("../data/schema.sql")).unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    fn insert_test_message(
        conn: &Connection,
        uuid: &str,
        session_id: &str,
        content: &str,
        msg_type: &str,
        timestamp: &str,
        project_path: &str,
    ) {
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, NULL, FALSE, 0, ?, ?, ?, 'test.jsonl', ?, FALSE, FALSE)",
            rusqlite::params![uuid, session_id, timestamp, msg_type, project_path, content],
        )
        .unwrap();
    }

    fn insert_test_conversation(
        conn: &Connection,
        session_id: &str,
        project_path: &str,
        summary: &str,
        first_at: &str,
        last_at: &str,
        source: &str,
    ) {
        conn.execute(
            "INSERT INTO conversations (session_id, project_path, conversation_file, root_message_uuid, conversation_summary, first_message_at, last_message_at, message_count, source) VALUES (?, ?, 'test.jsonl', 'root', ?, ?, ?, 1, ?)",
            rusqlite::params![session_id, project_path, summary, first_at, last_at, source],
        )
        .unwrap();
    }

    fn insert_test_conversation_with_repo(
        conn: &Connection,
        session_id: &str,
        project_path: &str,
        summary: &str,
        first_at: &str,
        last_at: &str,
        source: &str,
        repo_root: &str,
    ) {
        conn.execute(
            "INSERT INTO conversations (session_id, project_path, conversation_file, root_message_uuid, conversation_summary, first_message_at, last_message_at, message_count, source, repo_root) VALUES (?, ?, 'test.jsonl', 'root', ?, ?, ?, 1, ?, ?)",
            rusqlite::params![session_id, project_path, summary, first_at, last_at, source, repo_root],
        )
        .unwrap();
    }

    // ---- escape_like tests ----

    #[test]
    fn test_escape_like_percent() {
        assert_eq!(escape_like("%test%"), "\\%test\\%");
    }

    #[test]
    fn test_escape_like_underscore() {
        assert_eq!(escape_like("test_val"), "test\\_val");
    }

    #[test]
    fn test_escape_like_backslash() {
        assert_eq!(escape_like("a\\b"), "a\\\\b");
    }

    #[test]
    fn test_escape_like_no_special() {
        assert_eq!(escape_like("normal"), "normal");
    }

    // ---- format_timestamp tests ----

    #[test]
    fn test_format_timestamp_full() {
        let result = format_timestamp("2025-01-15T10:30:45Z", true, true);
        // Should contain date and seconds in local time
        assert!(result.contains("2025"));
        assert!(result.contains(":"));
        // Format: YYYY-MM-DD HH:MM:SS
        let parts: Vec<&str> = result.split(' ').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].matches(':').count(), 2); // HH:MM:SS has 2 colons
    }

    #[test]
    fn test_format_timestamp_time_only() {
        let result = format_timestamp("2025-01-15T10:30:45Z", false, false);
        // Should only have HH:MM, no date
        assert!(!result.contains("2025"));
        assert_eq!(result.matches(':').count(), 1); // HH:MM has 1 colon
    }

    #[test]
    fn test_format_timestamp_invalid() {
        let result = format_timestamp("not-a-timestamp", true, true);
        assert_eq!(result, "not-a-timestamp");
    }

    // ---- search tests ----

    #[test]
    fn test_search_empty_query() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "hello world", "user", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "goodbye world", "assistant", "2025-01-15T11:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("", &default_filter())
            .unwrap();

        assert_eq!(results.len(), 2);
        // Should be ordered by timestamp DESC
        assert!(results[0].timestamp >= results[1].timestamp);
    }

    #[test]
    fn test_search_fts_match() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "the quick brown fox jumps over the lazy dog", "user", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "rust programming language is great", "assistant", "2025-01-15T11:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("fox", &default_filter())
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_no_results() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "hello world", "user", "2025-01-15T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("xyzzyzzy", &default_filter())
            .unwrap();

        assert!(results.is_empty());
    }

    #[test]
    fn test_search_days_back_filter() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2020-01-01T00:00:00", "2020-01-01T01:00:00", "claude_code");
        insert_test_message(&conn, "old_msg", "sess1", "very old message content", "user", "2020-01-01T00:00:00", "/proj");

        insert_test_conversation(&conn, "sess2", "/proj", "summary2", "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code");
        // Use a timestamp that's definitely recent
        let recent_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        insert_test_message(&conn, "new_msg", "sess2", "brand new message content", "user", &recent_ts, "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("message content", &SearchFilter { days_back: Some(1), ..default_filter() })
            .unwrap();

        // Only the recent message should be returned
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "new_msg");
    }

    #[test]
    fn test_search_project_filter() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj_a", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_conversation(&conn, "sess2", "/proj_b", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "hello from project a", "user", "2025-01-15T10:00:00", "/proj_a");
        insert_test_message(&conn, "msg2", "sess2", "hello from project b", "user", "2025-01-15T10:00:00", "/proj_b");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("hello", &SearchFilter { project_path: Some("/proj_a"), ..default_filter() })
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].project_path.as_deref(), Some("/proj_a"));
    }

    #[test]
    fn test_search_source_filter() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_conversation(&conn, "sess2", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "opencode");
        insert_test_message(&conn, "msg1", "sess1", "message from claude", "user", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess2", "message from opencode", "user", "2025-01-15T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("message", &SearchFilter { source: Some("opencode"), ..default_filter() })
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source.as_deref(), Some("opencode"));
    }

    #[test]
    fn test_search_repo_filter() {
        let conn = setup_test_db();
        insert_test_conversation_with_repo(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code", "/home/user/my-repo");
        insert_test_conversation_with_repo(&conn, "sess2", "/proj2", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code", "/home/user/other-repo");
        insert_test_message(&conn, "msg1", "sess1", "code in my repo", "user", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess2", "code in other repo", "user", "2025-01-15T10:00:00", "/proj2");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("code", &SearchFilter { repo: Some("my-repo"), ..default_filter() })
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_date_filter() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-14T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "message on jan 15", "user", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "message on jan 14", "user", "2025-01-14T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("message", &SearchFilter { date: Some("2025-01-15"), ..default_filter() })
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_conflicting_filters() {
        let conn = setup_test_db();
        let mut searcher = ConversationSearch::from_connection(conn);

        let result = searcher.search_conversations(
            "test",
            &SearchFilter { days_back: Some(7), since: Some("2025-01-01"), ..default_filter() },
        );

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Cannot use --days"));
    }

    // ---- get_conversation_context tests ----

    #[test]
    fn test_get_conversation_context() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code");

        // Insert parent message
        insert_test_message(&conn, "parent1", "sess1", "parent message", "user", "2025-01-15T10:00:00", "/proj");

        // Insert child message with parent_uuid set
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, ?, FALSE, 1, ?, ?, ?, 'test.jsonl', ?, FALSE, FALSE)",
            rusqlite::params!["child1", "sess1", "parent1", "2025-01-15T10:01:00", "assistant", "/proj", "child message"],
        ).unwrap();

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_context("child1", 5).unwrap();

        assert!(result.message.is_some());
        assert!(result.conversation.is_some());

        assert_eq!(result.ancestors.len(), 1);
        assert_eq!(result.ancestors[0].message_uuid, "parent1");
    }

    #[test]
    fn test_get_conversation_context_not_found() {
        let conn = setup_test_db();
        let searcher = ConversationSearch::from_connection(conn);

        let result = searcher.get_conversation_context("nonexistent-uuid", 5).unwrap();

        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("not found"));
    }

    // ---- get_conversation_tree tests ----

    #[test]
    fn test_get_conversation_tree() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "tree test", "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code");

        // Insert root message
        insert_test_message(&conn, "root1", "sess1", "root message", "user", "2025-01-15T10:00:00", "/proj");

        // Insert child with parent_uuid
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, ?, FALSE, 1, ?, ?, ?, 'test.jsonl', ?, FALSE, FALSE)",
            rusqlite::params!["child1", "sess1", "root1", "2025-01-15T10:01:00", "assistant", "/proj", "child message"],
        ).unwrap();

        // Insert grandchild
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, ?, FALSE, 2, ?, ?, ?, 'test.jsonl', ?, FALSE, FALSE)",
            rusqlite::params!["grandchild1", "sess1", "child1", "2025-01-15T10:02:00", "user", "/proj", "grandchild message"],
        ).unwrap();

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess1").unwrap();

        assert!(result.conversation.is_some());
        assert_eq!(result.total_messages, 3);
        assert_eq!(result.tree.len(), 1); // One root

        // Root should have children
        assert_eq!(result.tree[0].children.len(), 1);

        // Child should have grandchild
        assert_eq!(result.tree[0].children[0].children.len(), 1);
    }

    // ---- list_recent_conversations tests ----

    #[test]
    fn test_list_recent_conversations() {
        let conn = setup_test_db();
        let recent_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        insert_test_conversation(&conn, "sess1", "/proj_a", "summary a", &recent_ts, &recent_ts, "claude_code");
        insert_test_conversation(&conn, "sess2", "/proj_b", "summary b", &recent_ts, &recent_ts, "opencode");

        let searcher = ConversationSearch::from_connection(conn);

        // List all recent
        let results = searcher
            .list_recent_conversations(&SearchFilter { days_back: Some(1), ..default_filter() })
            .unwrap();
        assert_eq!(results.len(), 2);

        // Filter by project
        let results = searcher
            .list_recent_conversations(&SearchFilter { days_back: Some(1), project_path: Some("/proj_a"), ..default_filter() })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].project_path.as_deref(), Some("/proj_a"));

        // Filter by source
        let results = searcher
            .list_recent_conversations(&SearchFilter { days_back: Some(1), source: Some("opencode"), ..default_filter() })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source.as_deref(), Some("opencode"));
    }

    // ---- query sanitization tests ----

    #[test]
    fn test_query_sanitization() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "rustacean programming language", "user", "2025-01-15T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);

        // Single term — trigram substring match
        let results = searcher
            .search_conversations("rustac", &default_filter())
            .unwrap();
        assert_eq!(results.len(), 1); // "rustac" is a substring of "rustacean"

        // Multi terms — AND join, each as substring match
        let results = searcher
            .search_conversations("rustac programm", &default_filter())
            .unwrap();
        assert_eq!(results.len(), 1); // both substrings found
    }

    // ---- Japanese / CJK search tests ----

    #[test]
    fn test_search_japanese_single_term() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "認証機能の実装を行いました", "assistant", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "hello world in english", "user", "2025-01-15T10:01:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("認証", &default_filter())
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_japanese_multi_term() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "認証機能の実装を行いました", "assistant", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "認証だけのメッセージ", "user", "2025-01-15T10:01:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        // Both terms must match (AND join)
        let results = searcher
            .search_conversations("認証 実装", &default_filter())
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1"); // Only msg1 contains both 認証 and 実装
    }

    #[test]
    fn test_search_mixed_cjk_english() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "OAuth認証の実装をRustで行いました", "assistant", "2025-01-15T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);

        // English term in mixed content
        let results = searcher
            .search_conversations("OAuth", &default_filter())
            .unwrap();
        assert_eq!(results.len(), 1);

        // Japanese term in mixed content
        let results = searcher
            .search_conversations("認証", &default_filter())
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_short_query_like_fallback() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "型の定義を変更しました", "assistant", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "英語のメッセージ", "user", "2025-01-15T10:01:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        // 1-byte short query (e.g. "ab") should use LIKE fallback
        let results = searcher
            .search_conversations("ab", &default_filter())
            .unwrap();
        assert!(results.is_empty()); // no match, but should not error

        // CJK 2-char query "型の" (6 bytes) should use LIKE fallback
        let results = searcher
            .search_conversations("型の", &default_filter())
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_cjk_2char_needs_like_fallback() {
        // SQLite trigram operates on codepoints, not bytes.
        // CJK 2-char terms (e.g. "認証") = 2 codepoints < 3, so FTS won't match.
        // LIKE fallback is required.
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "認証機能の実装を行いました", "assistant", "2025-01-15T10:00:00", "/proj");

        // Direct FTS MATCH with CJK 2-char term should NOT match
        let result: Option<String> = conn
            .query_row(
                "SELECT message_uuid FROM message_content_fts WHERE full_content MATCH '\"認証\"'",
                [],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(result, None, "CJK 2-char term should NOT match via FTS trigram (2 codepoints < 3)");

        // But search_conversations should still find it via LIKE fallback
        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("認証", &default_filter())
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_multi_term_and_join() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "rustacean programming language", "user", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "rustacean is a term for rust users", "user", "2025-01-15T10:01:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        // "rustac programm" should match msg1 (contains both substrings) but not msg2
        let results = searcher
            .search_conversations("rustac programm", &default_filter())
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_snippet_trigram() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "the quick brown fox jumps over the lazy dog", "user", "2025-01-15T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("brown fox", &default_filter())
            .unwrap();

        assert_eq!(results.len(), 1);
        let snippet = &results[0].context_snippet;
        // Snippet should contain highlighted search terms
        assert!(snippet.contains("**brown**"), "snippet should highlight 'brown': {}", snippet);
        assert!(snippet.contains("**fox**"), "snippet should highlight 'fox': {}", snippet);
    }
}
