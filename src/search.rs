use std::collections::HashMap;

use chrono::{DateTime, Local, TimeDelta, Utc};
use rusqlite::Connection;

use crate::date_utils::build_date_filter;
use crate::db;
use crate::error::{AppError, Result};

/// Escape special LIKE characters.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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
        days_back: Option<i64>,
        since: Option<&str>,
        until: Option<&str>,
        date: Option<&str>,
        limit: i64,
        project_path: Option<&str>,
        repo: Option<&str>,
        snippet_tokens: i32,
        source: Option<&str>,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>> {
        if days_back.is_some() && (since.is_some() || until.is_some() || date.is_some()) {
            return Err(AppError::General(
                "Cannot use --days with --since/--until/--date".to_string(),
            ));
        }

        let trimmed = query.trim();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        let sql = if trimmed.is_empty() {
            let mut sql = String::from(
                "SELECT m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, m.message_type, m.project_path, m.depth, m.is_sidechain, SUBSTR(m.full_content, 1, 500) as context_snippet, c.conversation_summary, c.conversation_file, c.source FROM messages m JOIN conversations c ON m.session_id = c.session_id WHERE m.is_meta_conversation = FALSE"
            );

            Self::append_filters(&mut sql, &mut params, days_back, since, until, date, project_path, repo, source)?;

            sql.push_str(" ORDER BY m.timestamp DESC LIMIT ?");
            params.push(Box::new(limit));
            sql
        } else {
            // Sanitize query for FTS5
            let fts_query = if !query.contains(" AND ")
                && !query.contains(" OR ")
                && !query.contains(" NOT ")
                && !query.contains('"')
            {
                let terms: Vec<&str> = query.split_whitespace().collect();
                if terms.len() == 1 {
                    format!("{}*", terms[0])
                } else {
                    terms.iter().map(|t| format!("{}*", t)).collect::<Vec<_>>().join(" ")
                }
            } else {
                query.to_string()
            };

            let mut sql = String::from(
                "SELECT m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, m.message_type, m.project_path, m.depth, m.is_sidechain, snippet(message_content_fts, 1, '**', '**', '...', ?) as context_snippet, c.conversation_summary, c.conversation_file, c.source FROM messages m JOIN message_content_fts ON m.rowid = message_content_fts.rowid JOIN conversations c ON m.session_id = c.session_id WHERE message_content_fts.full_content MATCH ? AND m.is_meta_conversation = FALSE"
            );

            params.push(Box::new(snippet_tokens));
            params.push(Box::new(fts_query));

            Self::append_filters(&mut sql, &mut params, days_back, since, until, date, project_path, repo, source)?;

            sql.push_str(" ORDER BY m.timestamp DESC LIMIT ?");
            params.push(Box::new(limit));
            sql
        };

        self.execute_search(&sql, &params)
    }

    fn append_filters(
        sql: &mut String,
        params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
        days_back: Option<i64>,
        since: Option<&str>,
        until: Option<&str>,
        date: Option<&str>,
        project_path: Option<&str>,
        repo: Option<&str>,
        source: Option<&str>,
    ) -> Result<()> {
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

    fn execute_search(
        &mut self,
        sql: &str,
        params: &[Box<dyn rusqlite::types::ToSql>],
    ) -> Result<Vec<HashMap<String, serde_json::Value>>> {
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        match self.query_to_maps(sql, &param_refs) {
            Ok(results) => Ok(results),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("fts5: missing row") && !self.fts_rebuilt {
                    eprintln!("FTS index corruption detected, rebuilding...");
                    self.rebuild_fts()?;
                    eprintln!("FTS index rebuilt, retrying search...");
                    self.query_to_maps(sql, &param_refs)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn query_to_maps(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::types::ToSql],
    ) -> Result<Vec<HashMap<String, serde_json::Value>>> {
        let mut stmt = self.conn.prepare(sql)?;
        let column_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|n| n.to_string())
            .collect();

        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            let mut map = HashMap::new();
            for (i, name) in column_names.iter().enumerate() {
                let val: rusqlite::types::Value = row.get(i)?;
                let json_val = match val {
                    rusqlite::types::Value::Null => serde_json::Value::Null,
                    rusqlite::types::Value::Integer(n) => serde_json::Value::Number(n.into()),
                    rusqlite::types::Value::Real(f) => {
                        serde_json::Value::Number(serde_json::Number::from_f64(f).unwrap_or(0.into()))
                    }
                    rusqlite::types::Value::Text(s) => serde_json::Value::String(s),
                    rusqlite::types::Value::Blob(b) => {
                        serde_json::Value::String(format!("<blob {} bytes>", b.len()))
                    }
                };
                map.insert(name.clone(), json_val);
            }
            Ok(map)
        })?;

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
    ) -> Result<HashMap<String, serde_json::Value>> {
        // Get target message
        let target = self.query_to_maps(
            "SELECT * FROM messages WHERE message_uuid = ?",
            &[&message_uuid as &dyn rusqlite::types::ToSql],
        )?;

        if target.is_empty() {
            let mut result = HashMap::new();
            result.insert(
                "error".to_string(),
                serde_json::Value::String(format!("Message {} not found", message_uuid)),
            );
            return Ok(result);
        }

        let target_msg = &target[0];

        // Walk up ancestors
        let mut ancestors = Vec::new();
        let mut current_uuid = target_msg
            .get("parent_uuid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut levels = 0;

        while let Some(ref uuid) = current_uuid {
            if levels >= depth {
                break;
            }
            let parent = self.query_to_maps(
                "SELECT * FROM messages WHERE message_uuid = ?",
                &[uuid as &dyn rusqlite::types::ToSql],
            )?;
            if parent.is_empty() {
                break;
            }
            ancestors.insert(0, serde_json::Value::Object(parent[0].clone().into_iter().collect()));
            current_uuid = parent[0]
                .get("parent_uuid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            levels += 1;
        }

        // Get conversation metadata
        let session_id = target_msg
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let conv = self.query_to_maps(
            "SELECT * FROM conversations WHERE session_id = ?",
            &[&session_id as &dyn rusqlite::types::ToSql],
        )?;

        let mut result = HashMap::new();
        result.insert(
            "message".to_string(),
            serde_json::Value::Object(target_msg.clone().into_iter().collect()),
        );
        result.insert("ancestors".to_string(), serde_json::Value::Array(ancestors));
        result.insert(
            "children".to_string(),
            serde_json::Value::Array(vec![]),
        );
        if !conv.is_empty() {
            result.insert(
                "conversation".to_string(),
                serde_json::Value::Object(conv[0].clone().into_iter().collect()),
            );
        }
        result.insert(
            "context_depth".to_string(),
            serde_json::Value::Number(levels.into()),
        );

        Ok(result)
    }

    pub fn get_conversation_tree(
        &self,
        session_id: &str,
    ) -> Result<HashMap<String, serde_json::Value>> {
        let messages = self.query_to_maps(
            "SELECT * FROM messages WHERE session_id = ? ORDER BY timestamp ASC",
            &[&session_id as &dyn rusqlite::types::ToSql],
        )?;

        let conv = self.query_to_maps(
            "SELECT * FROM conversations WHERE session_id = ?",
            &[&session_id as &dyn rusqlite::types::ToSql],
        )?;

        if conv.is_empty() {
            let mut result = HashMap::new();
            result.insert(
                "error".to_string(),
                serde_json::Value::String(format!("Conversation {} not found", session_id)),
            );
            return Ok(result);
        }

        let tree = self.build_tree(&messages);

        let mut result = HashMap::new();
        result.insert(
            "conversation".to_string(),
            serde_json::Value::Object(conv[0].clone().into_iter().collect()),
        );
        result.insert("tree".to_string(), serde_json::Value::Array(tree));
        result.insert(
            "total_messages".to_string(),
            serde_json::Value::Number(messages.len().into()),
        );

        Ok(result)
    }

    fn build_tree(
        &self,
        messages: &[HashMap<String, serde_json::Value>],
    ) -> Vec<serde_json::Value> {
        let mut msg_map: HashMap<String, serde_json::Map<String, serde_json::Value>> = HashMap::new();

        for msg in messages {
            let uuid = msg
                .get("message_uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut obj: serde_json::Map<String, serde_json::Value> = msg.clone().into_iter().collect();
            obj.insert("children".to_string(), serde_json::Value::Array(vec![]));
            msg_map.insert(uuid, obj);
        }

        let _uuids: Vec<String> = messages
            .iter()
            .filter_map(|m| m.get("message_uuid").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();

        let mut roots = Vec::new();
        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();

        for msg in messages {
            let uuid = msg
                .get("message_uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parent = msg
                .get("parent_uuid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if let Some(ref p) = parent {
                if msg_map.contains_key(p) {
                    children_map.entry(p.clone()).or_default().push(uuid);
                    continue;
                }
            }
            roots.push(uuid);
        }

        fn build_node(
            uuid: &str,
            msg_map: &HashMap<String, serde_json::Map<String, serde_json::Value>>,
            children_map: &HashMap<String, Vec<String>>,
        ) -> serde_json::Value {
            let mut node = msg_map.get(uuid).cloned().unwrap_or_default();
            if let Some(kids) = children_map.get(uuid) {
                let children: Vec<serde_json::Value> = kids
                    .iter()
                    .map(|k| build_node(k, msg_map, children_map))
                    .collect();
                node.insert("children".to_string(), serde_json::Value::Array(children));
            }
            serde_json::Value::Object(node)
        }

        roots
            .iter()
            .map(|uuid| build_node(uuid, &msg_map, &children_map))
            .collect()
    }

    pub fn list_recent_conversations(
        &self,
        days_back: Option<i64>,
        since: Option<&str>,
        until: Option<&str>,
        date: Option<&str>,
        limit: i64,
        project_path: Option<&str>,
        repo: Option<&str>,
        source: Option<&str>,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>> {
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
        self.query_to_maps(&sql, &param_refs)
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
        let conversations = self.query_to_maps(&sql, &param_refs)?;

        if conversations.is_empty() {
            return Ok(format!("No conversations found in the last {} day(s).", days_back));
        }

        let day_word = if days_back == 1 { "" } else { "s" };
        let mut lines = vec![format!("# Conversations (last {} day{})\n", days_back, day_word)];

        for conv in &conversations {
            let session_id = conv.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
            let summary = conv.get("conversation_summary").and_then(|v| v.as_str()).unwrap_or("");
            let project = conv.get("project_path").and_then(|v| v.as_str()).unwrap_or("");
            let msg_count = conv.get("message_count").and_then(|v| v.as_i64()).unwrap_or(0);
            let last_at = conv.get("last_message_at").and_then(|v| v.as_str()).unwrap_or("");

            let date_str = format_timestamp(last_at, true, false);
            let _time_str = format_timestamp(last_at, false, false);
            let session_short = &session_id[..std::cmp::min(8, session_id.len())];

            lines.push(format!("## [{}] {}", session_short, summary));
            lines.push(format!("**{} msgs** | {} | {}\n", msg_count, project, date_str));

            // Fetch messages
            let msg_sql = "SELECT message_uuid, timestamp, message_type, summary, is_sidechain, project_path, is_tool_noise FROM messages WHERE session_id = ? AND is_tool_noise = FALSE AND is_meta_conversation = FALSE ORDER BY timestamp DESC LIMIT ?";
            let msg_results = self.query_to_maps(
                msg_sql,
                &[&session_id as &dyn rusqlite::types::ToSql, &max_messages_per_conv as &dyn rusqlite::types::ToSql],
            )?;

            let mut msg_results = msg_results;
            msg_results.reverse();

            for msg in &msg_results {
                let msg_summary = msg.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                if msg_summary.is_empty()
                    || msg_summary.starts_with("[Tool")
                    || msg_summary == "[Tool result]"
                    || msg_summary.starts_with("[Request interrupted")
                    || msg_summary.trim().len() < 10
                {
                    continue;
                }

                let msg_time = format_timestamp(
                    msg.get("timestamp").and_then(|v| v.as_str()).unwrap_or(""),
                    false,
                    false,
                );
                let icon = if msg.get("message_type").and_then(|v| v.as_str()) == Some("user") {
                    "\u{1f464}"
                } else {
                    "\u{1f916}"
                };
                let is_sidechain = msg.get("is_sidechain").and_then(|v| v.as_i64()).unwrap_or(0) != 0;
                let branch = if is_sidechain { "\u{1f33f} " } else { "" };
                let uuid_short = &msg
                    .get("message_uuid")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")[..8.min(msg.get("message_uuid").and_then(|v| v.as_str()).unwrap_or("").len())];

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
            .search_conversations("", None, None, None, None, 10, None, None, 32, None)
            .unwrap();

        assert_eq!(results.len(), 2);
        // Should be ordered by timestamp DESC
        let ts0 = results[0].get("timestamp").and_then(|v| v.as_str()).unwrap();
        let ts1 = results[1].get("timestamp").and_then(|v| v.as_str()).unwrap();
        assert!(ts0 >= ts1);
    }

    #[test]
    fn test_search_fts_match() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "the quick brown fox jumps over the lazy dog", "user", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "rust programming language is great", "assistant", "2025-01-15T11:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("fox", None, None, None, None, 10, None, None, 32, None)
            .unwrap();

        assert_eq!(results.len(), 1);
        let uuid = results[0].get("message_uuid").and_then(|v| v.as_str()).unwrap();
        assert_eq!(uuid, "msg1");
    }

    #[test]
    fn test_search_no_results() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "hello world", "user", "2025-01-15T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("xyzzyzzy", None, None, None, None, 10, None, None, 32, None)
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
            .search_conversations("message content", Some(1), None, None, None, 10, None, None, 32, None)
            .unwrap();

        // Only the recent message should be returned
        assert_eq!(results.len(), 1);
        let uuid = results[0].get("message_uuid").and_then(|v| v.as_str()).unwrap();
        assert_eq!(uuid, "new_msg");
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
            .search_conversations("hello", None, None, None, None, 10, Some("/proj_a"), None, 32, None)
            .unwrap();

        assert_eq!(results.len(), 1);
        let pp = results[0].get("project_path").and_then(|v| v.as_str()).unwrap();
        assert_eq!(pp, "/proj_a");
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
            .search_conversations("message", None, None, None, None, 10, None, None, 32, Some("opencode"))
            .unwrap();

        assert_eq!(results.len(), 1);
        let source = results[0].get("source").and_then(|v| v.as_str()).unwrap();
        assert_eq!(source, "opencode");
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
            .search_conversations("code", None, None, None, None, 10, None, Some("my-repo"), 32, None)
            .unwrap();

        assert_eq!(results.len(), 1);
        let uuid = results[0].get("message_uuid").and_then(|v| v.as_str()).unwrap();
        assert_eq!(uuid, "msg1");
    }

    #[test]
    fn test_search_date_filter() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-14T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "message on jan 15", "user", "2025-01-15T10:00:00", "/proj");
        insert_test_message(&conn, "msg2", "sess1", "message on jan 14", "user", "2025-01-14T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("message", None, None, None, Some("2025-01-15"), 10, None, None, 32, None)
            .unwrap();

        assert_eq!(results.len(), 1);
        let uuid = results[0].get("message_uuid").and_then(|v| v.as_str()).unwrap();
        assert_eq!(uuid, "msg1");
    }

    #[test]
    fn test_search_conflicting_filters() {
        let conn = setup_test_db();
        let mut searcher = ConversationSearch::from_connection(conn);

        let result = searcher.search_conversations(
            "test",
            Some(7),
            Some("2025-01-01"),
            None,
            None,
            10,
            None,
            None,
            32,
            None,
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

        assert!(result.contains_key("message"));
        assert!(result.contains_key("ancestors"));
        assert!(result.contains_key("conversation"));

        let ancestors = result.get("ancestors").unwrap().as_array().unwrap();
        assert_eq!(ancestors.len(), 1);

        let ancestor_uuid = ancestors[0].get("message_uuid").and_then(|v| v.as_str()).unwrap();
        assert_eq!(ancestor_uuid, "parent1");
    }

    #[test]
    fn test_get_conversation_context_not_found() {
        let conn = setup_test_db();
        let searcher = ConversationSearch::from_connection(conn);

        let result = searcher.get_conversation_context("nonexistent-uuid", 5).unwrap();

        assert!(result.contains_key("error"));
        let err_msg = result.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(err_msg.contains("not found"));
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

        assert!(result.contains_key("conversation"));
        assert!(result.contains_key("tree"));
        assert!(result.contains_key("total_messages"));

        let total = result.get("total_messages").and_then(|v| v.as_i64()).unwrap();
        assert_eq!(total, 3);

        let tree = result.get("tree").unwrap().as_array().unwrap();
        assert_eq!(tree.len(), 1); // One root

        // Root should have children
        let root_children = tree[0].get("children").unwrap().as_array().unwrap();
        assert_eq!(root_children.len(), 1);

        // Child should have grandchild
        let child_children = root_children[0].get("children").unwrap().as_array().unwrap();
        assert_eq!(child_children.len(), 1);
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
            .list_recent_conversations(Some(1), None, None, None, 10, None, None, None)
            .unwrap();
        assert_eq!(results.len(), 2);

        // Filter by project
        let results = searcher
            .list_recent_conversations(Some(1), None, None, None, 10, Some("/proj_a"), None, None)
            .unwrap();
        assert_eq!(results.len(), 1);
        let pp = results[0].get("project_path").and_then(|v| v.as_str()).unwrap();
        assert_eq!(pp, "/proj_a");

        // Filter by source
        let results = searcher
            .list_recent_conversations(Some(1), None, None, None, 10, None, None, Some("opencode"))
            .unwrap();
        assert_eq!(results.len(), 1);
        let src = results[0].get("source").and_then(|v| v.as_str()).unwrap();
        assert_eq!(src, "opencode");
    }

    // ---- query sanitization tests ----

    #[test]
    fn test_query_sanitization() {
        let conn = setup_test_db();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", "2025-01-15T09:00:00", "2025-01-15T10:00:00", "claude_code");
        insert_test_message(&conn, "msg1", "sess1", "rustacean programming language", "user", "2025-01-15T10:00:00", "/proj");

        let mut searcher = ConversationSearch::from_connection(conn);

        // Single term should get * appended (prefix search)
        let results = searcher
            .search_conversations("rustac", None, None, None, None, 10, None, None, 32, None)
            .unwrap();
        assert_eq!(results.len(), 1); // "rustac*" matches "rustacean"

        // Multi terms should each get *
        let results = searcher
            .search_conversations("rustac programm", None, None, None, None, 10, None, None, 32, None)
            .unwrap();
        assert_eq!(results.len(), 1); // "rustac* programm*" matches
    }
}
