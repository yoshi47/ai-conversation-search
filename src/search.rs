use std::collections::HashMap;

use chrono::{DateTime, Local, TimeDelta, Utc};
use rusqlite::Connection;
use serde::Serialize;

use crate::date_utils::build_date_filter;
use crate::db;
use crate::error::{AppError, Result};
use crate::indexer::{ConversationIndexer, Message};

/// Result ordering for text search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// bm25 relevance, best-first. Default for `search`.
    Relevance,
    /// Newest first. The only meaningful order for `list`.
    Recent,
}

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
    pub sort: SortOrder,
}

impl Default for SearchFilter<'_> {
    fn default() -> Self {
        Self {
            days_back: None,
            since: None,
            until: None,
            date: None,
            limit: 20,
            project_path: None,
            repo: None,
            source: None,
            sort: SortOrder::Relevance,
        }
    }
}

/// A single search result row (messages JOIN conversations).
#[derive(Debug, Clone, Serialize)]
pub struct SearchResultRow {
    // Skipped in JSON: the rowid is an internal join key for bm25 scores, and
    // `--json` output is a CLI contract consumed by bin/ai-conversation-search.
    #[serde(skip)]
    pub rowid: i64,
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
            rowid: row.get("message_rowid")?,
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
    pub warning: Option<String>,
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

/// Search statistics for verbose output.
#[derive(Debug, Clone, Serialize)]
pub struct SearchStats {
    pub total_indexed_sessions: i64,
    pub total_indexed_messages: i64,
    pub sessions_in_scope: i64,
    pub matched_messages: i64,
    /// Whether `limit` cut off results that otherwise matched. A lower bound on the FTS
    /// path, exact elsewhere -- see the assignment sites.
    pub truncated: bool,
}

/// Search result with statistics.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub rows: Vec<SearchResultRow>,
    pub stats: SearchStats,
}

/// `list` rows plus whether `--limit` cut off the tail.
///
/// Deliberately not `SearchStats`: `gather_search_stats` counts message-scoped totals via
/// `append_filters`, which filters on `messages.timestamp`, while `list` filters on
/// `conversations.last_message_at`. Reusing it would attach three extra COUNT queries whose
/// numbers answer a different question.
#[derive(Debug, Clone, Serialize)]
pub struct ListResult {
    pub rows: Vec<ConversationRow>,
    pub truncated: bool,
}

/// One session's representative row plus its match count.
#[derive(Debug, Clone, Serialize)]
pub struct GroupedRow {
    #[serde(flatten)]
    pub representative: SearchResultRow,
    pub match_count: i64,
}

/// Grouped-by-session search result.
#[derive(Debug, Clone, Serialize)]
pub struct GroupedSearchResult {
    pub rows: Vec<GroupedRow>,
    pub stats: SearchStats,
}

/// Escape special LIKE characters.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The trigram tokenizer emits no tokens for shorter terms, so they match nothing in FTS.
const MIN_TRIGRAM_CHARS: usize = 3;

/// How a query maps onto the FTS and LIKE machinery.
enum QueryPlan<'a> {
    /// Hand the query to FTS verbatim: quoted phrases and explicit operators.
    Raw(String),
    /// Terms of `MIN_TRIGRAM_CHARS`+ go to FTS, OR-joined and bm25-ranked. Shorter
    /// terms cannot be ranked by, so they narrow the phase-2 rows with LIKE instead.
    Hybrid {
        fts_query: String,
        short_terms: Vec<&'a str>,
    },
    /// Nothing reaches the trigram minimum. Full-table LIKE, AND semantics, recency
    /// order. Also covers the empty query, where `terms` is empty.
    LikeOnly { terms: Vec<&'a str> },
}

fn plan_query(trimmed: &str) -> QueryPlan<'_> {
    // Quoting is the documented escape hatch: it reaches FTS whatever the term lengths.
    if trimmed.contains('"') {
        return QueryPlan::Raw(trimmed.to_string());
    }

    let terms: Vec<&str> = trimmed.split_whitespace().collect();
    let (long_terms, short_terms): (Vec<&str>, Vec<&str>) = terms
        .iter()
        .copied()
        .partition(|t| t.chars().count() >= MIN_TRIGRAM_CHARS);

    // Why not test for operators first: `OR` is two characters, so an all-short query
    // such as `更新 OR 認証` would reach FTS with no tokenizable term and return zero
    // rows. LikeOnly treats the operator as a literal, which is imperfect but non-empty.
    if long_terms.is_empty() {
        return QueryPlan::LikeOnly { terms };
    }

    // Spaces are load-bearing: a bare `contains("AND")` would misroute `STANDARD`.
    if trimmed.contains(" AND ") || trimmed.contains(" OR ") || trimmed.contains(" NOT ") {
        // An explicit-operator query goes to FTS whole, so a sub-trigram term inside it
        // cannot be lifted out into a LIKE the way Hybrid does -- and it would tokenize to
        // nothing, silently changing what the operator means (`A AND 失敗` matches zero
        // rows, `A NOT 失敗` excludes nothing). LikeOnly can't express the operator either,
        // but it treats it as a literal and keeps every term in play. `OR` is excluded from
        // the test because it is itself two characters.
        let has_short_operand = short_terms
            .iter()
            .any(|t| !matches!(*t, "AND" | "OR" | "NOT"));
        if has_short_operand {
            return QueryPlan::LikeOnly { terms };
        }
        return QueryPlan::Raw(trimmed.to_string());
    }

    QueryPlan::Hybrid {
        fts_query: long_terms
            .iter()
            .map(|t| format!("\"{}\"", t))
            .collect::<Vec<_>>()
            .join(" OR "),
        short_terms,
    }
}

/// Extract a snippet around the first occurrence of any search term in content.
/// Returns a window of `max_len` chars centered on the match, with `**` highlighting.
fn extract_snippet(content: &str, search_terms: &[&str], max_len: usize) -> String {
    // Find the earliest match position
    let mut best_pos: Option<(usize, usize)> = None; // (byte_start, byte_len)
    for term in search_terms {
        if let Some((byte_start, byte_end)) = find_term(content, term) {
            match best_pos {
                None => best_pos = Some((byte_start, byte_end - byte_start)),
                Some((prev, _)) if byte_start < prev => {
                    best_pos = Some((byte_start, byte_end - byte_start))
                }
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
        let mut highlighted = String::new();
        let mut remaining = result.as_str();
        while !remaining.is_empty() {
            if let Some((start, end)) = find_term(remaining, term) {
                highlighted.push_str(&remaining[..start]);
                let matched = &remaining[start..end];
                highlighted.push_str("**");
                highlighted.push_str(matched);
                highlighted.push_str("**");
                remaining = &remaining[end..];
            } else {
                highlighted.push_str(remaining);
                break;
            }
        }
        result = highlighted;
    }

    let prefix = if window_start > 0 { "..." } else { "" };
    let suffix = if window_start + max_len < content.chars().count() {
        "..."
    } else {
        ""
    };
    format!("{}{}{}", prefix, result, suffix)
}

fn find_term(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }

    let needle_lower = needle.to_lowercase();
    let mut haystack_lower = String::with_capacity(haystack.len());
    let mut original_ranges = Vec::with_capacity(haystack.len());

    for (start, ch) in haystack.char_indices() {
        let end = start + ch.len_utf8();
        for lower_ch in ch.to_lowercase() {
            let mut buf = [0; 4];
            let lower_str = lower_ch.encode_utf8(&mut buf);
            haystack_lower.push_str(lower_str);
            for _ in 0..lower_str.len() {
                original_ranges.push((start, end));
            }
        }
    }

    if let Some(lower_start) = haystack_lower.find(&needle_lower) {
        let lower_end = lower_start + needle_lower.len();
        let original_start = original_ranges[lower_start].0;
        let original_end = original_ranges[lower_end - 1].1;
        return Some((original_start, original_end));
    }

    None
}

fn summary_from_content(content: &str) -> String {
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    first_line.chars().take(120).collect()
}

/// Convert UTC ISO timestamp to local time for display.
pub fn format_timestamp(iso_timestamp: &str, include_date: bool, include_seconds: bool) -> String {
    let cleaned = iso_timestamp.replace('Z', "+00:00");
    let dt_utc = match DateTime::parse_from_rfc3339(&cleaned) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(_) => {
            // Try parsing without timezone
            if let Ok(naive) =
                chrono::NaiveDateTime::parse_from_str(iso_timestamp, "%Y-%m-%dT%H:%M:%S%.f")
            {
                naive.and_utc()
            } else if let Ok(naive) =
                chrono::NaiveDateTime::parse_from_str(iso_timestamp, "%Y-%m-%dT%H:%M:%S")
            {
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

/// Source breakdown entry for index status.
#[derive(Debug, Clone, Serialize)]
pub struct SourceCount {
    pub source: String,
    pub count: i64,
}

/// Repository breakdown entry for index status.
#[derive(Debug, Clone, Serialize)]
pub struct RepoCount {
    pub repo_root: String,
    pub count: i64,
}

/// Index status information.
#[derive(Debug, Clone, Serialize)]
pub struct IndexStatus {
    pub total_conversations: i64,
    pub total_messages: i64,
    pub orphan_conversations: i64,
    pub earliest_conversation: Option<String>,
    pub latest_conversation: Option<String>,
    pub by_source: Vec<SourceCount>,
    pub by_repo: Vec<RepoCount>,
    pub db_size_bytes: u64,
    pub indexed_files: i64,
    pub files_on_disk: usize,
    pub fts_healthy: bool,
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

    pub fn count_indexed_files(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM claude_code_sync_state", [], |r| {
                r.get(0)
            })?)
    }

    pub fn get_index_status(&self, files_on_disk: usize) -> Result<IndexStatus> {
        let total_conversations: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?;

        let total_messages: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;

        let orphan_conversations: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT c.session_id
                FROM conversations c
                LEFT JOIN messages m ON m.session_id = c.session_id
                WHERE c.message_count > 0
                  AND COALESCE(c.source, 'claude_code') = 'claude_code'
                GROUP BY c.session_id
                HAVING COUNT(m.message_uuid) = 0
            )",
            [],
            |r| r.get(0),
        )?;

        let earliest_conversation: Option<String> =
            self.conn
                .query_row("SELECT MIN(first_message_at) FROM conversations", [], |r| {
                    r.get(0)
                })?;

        let latest_conversation: Option<String> =
            self.conn
                .query_row("SELECT MAX(last_message_at) FROM conversations", [], |r| {
                    r.get(0)
                })?;

        let mut by_source = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT COALESCE(source, 'claude_code'), COUNT(*) FROM conversations GROUP BY source ORDER BY COUNT(*) DESC"
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(SourceCount {
                    source: row.get(0)?,
                    count: row.get(1)?,
                })
            })?;
            for row in rows.flatten() {
                by_source.push(row);
            }
        }

        let mut by_repo = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT repo_root, COUNT(*) as cnt FROM conversations WHERE repo_root IS NOT NULL GROUP BY repo_root ORDER BY cnt DESC LIMIT 20"
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(RepoCount {
                    repo_root: row.get(0)?,
                    count: row.get(1)?,
                })
            })?;
            for row in rows.flatten() {
                by_repo.push(row);
            }
        }

        let indexed_files: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM claude_code_sync_state", [], |r| {
                    r.get(0)
                })?;

        let db_path = db::expand_path(&self.db_path);
        let db_size_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

        // rank=1 makes the check compare the index against the content table. The
        // argument-less form only validates the index's internal structure, which cannot
        // see entries stranded by the pre-0.15.0 delete trigger -- the exact damage this
        // check most needs to surface.
        //
        // eprintln rather than log::warn: env_logger defaults to Error level, so a warn
        // would leave "CORRUPTED" on screen with no reason attached.
        let fts_healthy = match db::connect(&self.db_path, false) {
            Ok(rw_conn) => match rw_conn.execute(
                "INSERT INTO message_content_fts(message_content_fts, rank) VALUES('integrity-check', 1)",
                [],
            ) {
                Ok(_) => true,
                Err(e) => {
                    eprintln!("FTS integrity check failed: {}", e);
                    eprintln!("Run 'ai-conversation-search prune-observer' to rebuild the index.");
                    false
                }
            },
            Err(e) => {
                // Not a corruption signal: the database just could not be opened for
                // writing (permissions, another process holding it, read-only mount).
                eprintln!("Could not verify FTS health (database not writable): {}", e);
                true
            }
        };

        Ok(IndexStatus {
            total_conversations,
            total_messages,
            orphan_conversations,
            earliest_conversation,
            latest_conversation,
            by_source,
            by_repo,
            db_size_bytes,
            indexed_files,
            files_on_disk,
            fts_healthy,
        })
    }

    /// Gather search statistics (total indexed and in-scope counts).
    fn gather_search_stats(
        &self,
        filter: &SearchFilter<'_>,
        matched: i64,
        truncated: bool,
    ) -> Result<SearchStats> {
        let total_indexed_sessions: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?;

        let total_indexed_messages: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE is_meta_conversation = FALSE",
            [],
            |r| r.get(0),
        )?;

        // Count sessions in scope (after filters)
        let mut sql = String::from(
            "SELECT COUNT(DISTINCT m.session_id) FROM messages m JOIN conversations c ON m.session_id = c.session_id WHERE m.is_meta_conversation = FALSE"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        Self::append_filters(&mut sql, &mut params, filter)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let sessions_in_scope: i64 = self.conn.prepare(&sql).and_then(|mut stmt| {
            stmt.query_row(rusqlite::params_from_iter(&param_refs), |r| r.get(0))
        })?;

        Ok(SearchStats {
            total_indexed_sessions,
            total_indexed_messages,
            sessions_in_scope,
            matched_messages: matched,
            truncated,
        })
    }

    pub fn search_conversations(
        &mut self,
        query: &str,
        filter: &SearchFilter<'_>,
    ) -> Result<SearchResult> {
        let days_back = filter.days_back;
        let since = filter.since;
        let until = filter.until;
        let date = filter.date;
        let limit = filter.limit;

        if days_back.is_some() && (since.is_some() || until.is_some() || date.is_some()) {
            return Err(AppError::General(
                "Cannot use --days with --since/--until/--date".to_string(),
            ));
        }

        // Rejected rather than clamped: a negative limit means two different wrong things
        // depending on the path -- SQLite reads `LIMIT -2` and below as unbounded, while
        // `truncate(limit as usize)` wraps to usize::MAX and drops nothing. Mirrors the
        // guard in search_grouped_by_session.
        if limit < 0 {
            return Err(AppError::General(format!(
                "limit must be >= 0, got {}",
                limit
            )));
        }

        let trimmed = query.trim();

        let (fts_query, short_terms) = match plan_query(trimmed) {
            QueryPlan::LikeOnly { terms } => {
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                // No term reaches the trigram minimum, so there is no term-frequency signal
                // to rank by. This path keeps AND semantics and recency order, unlike the FTS
                // path (OR + bm25): a 2-character term ORed against anything matches nearly
                // the whole corpus, and without ranking that is pure noise. Deliberate.
                let mut sql = String::from(
                    "SELECT m.rowid AS message_rowid, m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, m.message_type, m.project_path, m.depth, m.is_sidechain, SUBSTR(m.full_content, 1, 500) as context_snippet, c.conversation_summary, c.conversation_file, c.source FROM messages m JOIN conversations c ON m.session_id = c.session_id WHERE m.is_meta_conversation = FALSE"
                );

                for term in &terms {
                    sql.push_str(" AND m.full_content LIKE ? ESCAPE '\\'");
                    params.push(Box::new(format!("%{}%", escape_like(term))));
                }

                Self::append_filters(&mut sql, &mut params, filter)?;

                // Over-fetch by one: SQL caps the result set, so the extra row is the only
                // way to tell "exactly `limit` matches" from "more were cut off".
                sql.push_str(" ORDER BY m.timestamp DESC LIMIT ?");
                params.push(Box::new(limit.saturating_add(1)));

                let mut rows = self.execute_search_typed(&sql, &params)?;
                let truncated = rows.len() as i64 > limit;
                rows.truncate(limit as usize);
                let matched = rows.len() as i64;
                let stats = self.gather_search_stats(filter, matched, truncated)?;
                return Ok(SearchResult { rows, stats });
            }
            QueryPlan::Raw(q) => (q, Vec::new()),
            QueryPlan::Hybrid {
                fts_query,
                short_terms,
            } => (fts_query, short_terms),
        };

        // Two-phase query to work around SQLite trigram FTS performance issue.
        // SQLite's planner incorrectly uses idx_is_meta_conversation as the driving index,
        // scanning ~all messages and checking trigram FTS for each row (O(N) full table scan).
        // Phase 1: Get matching rowids from FTS (fast, ~50ms for 1700 matches).
        // Phase 2: Query messages+conversations by rowid IN batches.
        let scored = self.query_fts_rowids(&fts_query)?;

        if scored.is_empty() {
            let stats = self.gather_search_stats(filter, 0, false)?;
            return Ok(SearchResult {
                rows: Vec::new(),
                stats,
            });
        }

        let score_by_rowid: HashMap<i64, f64> = scored.iter().copied().collect();

        // Process in batches to stay within SQLITE_MAX_VARIABLE_NUMBER
        const BATCH_SIZE: usize = 500;
        let mut all_results: Vec<SearchResultRow> = Vec::new();
        let mut candidates_scanned = 0usize;

        for chunk in scored.chunks(BATCH_SIZE) {
            candidates_scanned += chunk.len();
            let mut batch_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut sql = format!(
                "SELECT m.rowid AS message_rowid, m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, m.message_type, m.project_path, m.depth, m.is_sidechain, m.full_content as context_snippet, c.conversation_summary, c.conversation_file, c.source FROM messages m JOIN conversations c ON m.session_id = c.session_id WHERE m.rowid IN ({}) AND m.is_meta_conversation = FALSE",
                placeholders
            );

            for (rowid, _) in chunk {
                batch_params.push(Box::new(*rowid));
            }

            // Terms under the trigram minimum contribute nothing to the FTS match, so
            // they narrow here instead. Like every other phase-2 predicate this is
            // purely subtractive, which is what keeps the early stop below sound.
            // Why LIKE rather than a Rust-side retain over the already-loaded
            // full_content: LIKE is ASCII case-insensitive and `str::contains` is not,
            // and a short term must mean the same thing on both search paths.
            for term in &short_terms {
                sql.push_str(" AND m.full_content LIKE ? ESCAPE '\\'");
                batch_params.push(Box::new(format!("%{}%", escape_like(term))));
            }

            Self::append_filters(&mut sql, &mut batch_params, filter)?;
            // No ORDER BY: the final order is decided in Rust below, because the
            // bm25 score lives in phase 1 and is not visible to this query.

            let batch_results = self.execute_search_typed(&sql, &batch_params)?;
            all_results.extend(batch_results);

            // Early stop, valid ONLY for relevance order.
            //
            // WRONG alternative: truncating the phase-1 rowid list to `limit` before
            // phase 2. Filters (project/date/source/repo) and is_meta_conversation are
            // applied only in phase 2, so if the top hits all belong to another project
            // that would return zero rows while the correct answer is non-empty.
            //
            // What holds instead: phase 1 returns rowids sorted best-first, so after
            // processing batches 0..k every unprocessed candidate scores no better than
            // what we already hold. Once we have `limit` post-filter rows, they are the
            // globally best `limit` rows -- filters only remove candidates, never promote
            // them, so this is independent of which filters are active.
            //
            // Does not hold for Recent order: recency is uncorrelated with bm25 rank.
            if filter.sort == SortOrder::Relevance && all_results.len() >= limit as usize {
                break;
            }
        }

        // bm25's length normalization is what sinks the multi-KB observer transcripts,
        // but it could in principle let a one-line fragment outrank a substantive
        // discussion. If that shows up, the fix is a length-aware tie-break here
        // (`(bm25, -len)`), not a length filter -- a hard floor would make
        // short-but-correct messages unfindable.
        match filter.sort {
            SortOrder::Relevance => all_results.sort_by(|a, b| {
                let sa = score_by_rowid.get(&a.rowid).copied().unwrap_or(f64::MAX);
                let sb = score_by_rowid.get(&b.rowid).copied().unwrap_or(f64::MAX);
                sa.partial_cmp(&sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    // Ties prefer the newer message AMONG COLLECTED ROWS ONLY. An
                    // equal-scoring row in a later batch is never fetched once the
                    // early stop fires, so across a batch boundary ties resolve in
                    // phase-1 order instead. Accepted: exact bm25 ties are rare, and
                    // closing the gap means scanning past `limit` on every query.
                    .then_with(|| b.timestamp.cmp(&a.timestamp))
            }),
            SortOrder::Recent => all_results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
        }
        // Collecting more than `limit` proves rows were dropped. The early stop can also
        // land on exactly `limit` while candidates remain unscanned; those leftovers may
        // all fail the filters, so warning there can overstate -- but the reverse mistake,
        // staying silent about real matches, is what makes a caller read a capped list as
        // "nothing else exists". Over-warn rather than under-warn.
        let truncated = all_results.len() > limit as usize || candidates_scanned < scored.len();
        all_results.truncate(limit as usize);

        // Post-process: extract snippets with highlighting around match locations
        let search_terms: Vec<&str> = trimmed.split_whitespace().collect();
        for row in &mut all_results {
            row.context_snippet = extract_snippet(&row.context_snippet, &search_terms, 200);
        }
        let matched = all_results.len() as i64;
        let stats = self.gather_search_stats(filter, matched, truncated)?;
        Ok(SearchResult {
            rows: all_results,
            stats,
        })
    }

    /// Search and group results by session, with `limit` applied at the SESSION level
    /// (not the message level). Returns up to `limit` distinct sessions and the total match
    /// count for each. The representative message is the session's best-scoring match under
    /// `SortOrder::Relevance`. Under `SortOrder::Recent` -- and on the LIKE-only path
    /// (empty query, or no term reaching 3 characters) regardless of `sort`, since that
    /// path has no bm25 score to rank by -- it is the most recent match instead.
    pub fn search_grouped_by_session(
        &mut self,
        query: &str,
        filter: &SearchFilter<'_>,
    ) -> Result<GroupedSearchResult> {
        let days_back = filter.days_back;
        let since = filter.since;
        let until = filter.until;
        let date = filter.date;
        let limit = filter.limit;

        if days_back.is_some() && (since.is_some() || until.is_some() || date.is_some()) {
            return Err(AppError::General(
                "Cannot use --days with --since/--until/--date".to_string(),
            ));
        }
        if limit < 0 {
            return Err(AppError::General(format!(
                "limit must be >= 0, got {}",
                limit
            )));
        }
        let limit_usize = limit as usize;

        let trimmed = query.trim();
        let plan = plan_query(trimmed);

        let (fts_query, short_terms) = match plan {
            QueryPlan::LikeOnly { terms } => {
                // SQL window function picks the most recent message per session and counts
                // matches per session, then we LIMIT at session level. Like the non-grouped
                // LikeOnly path, this keeps AND + recency: no FTS query means no bm25.
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                let mut inner_sql = String::from(
                    "SELECT m.rowid AS message_rowid, m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, \
                     m.message_type, m.project_path, m.depth, m.is_sidechain, \
                     SUBSTR(m.full_content, 1, 500) AS context_snippet, \
                     c.conversation_summary, c.conversation_file, c.source, \
                     ROW_NUMBER() OVER (PARTITION BY m.session_id ORDER BY m.timestamp DESC) AS rn, \
                     COUNT(*) OVER (PARTITION BY m.session_id) AS match_count \
                     FROM messages m JOIN conversations c ON m.session_id = c.session_id \
                     WHERE m.is_meta_conversation = FALSE",
                );

                for term in &terms {
                    inner_sql.push_str(" AND m.full_content LIKE ? ESCAPE '\\'");
                    params.push(Box::new(format!("%{}%", escape_like(term))));
                }

                Self::append_filters(&mut inner_sql, &mut params, filter)?;

                let sql = format!(
                    "WITH ranked AS ({}) SELECT * FROM ranked WHERE rn = 1 \
                     ORDER BY timestamp DESC LIMIT ?",
                    inner_sql
                );
                // Over-fetch by one to distinguish "exactly `limit` sessions" from "capped".
                params.push(Box::new(limit.saturating_add(1)));

                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();
                let mut rows = self.query_rows(&sql, &param_refs, |row| {
                    Ok(GroupedRow {
                        representative: SearchResultRow::from_row(row)?,
                        match_count: row.get("match_count")?,
                    })
                })?;

                let truncated = rows.len() as i64 > limit;
                rows.truncate(limit_usize);
                let matched_total: i64 = rows.iter().map(|g| g.match_count).sum();
                let stats = self.gather_search_stats(filter, matched_total, truncated)?;
                return Ok(GroupedSearchResult { rows, stats });
            }
            QueryPlan::Raw(q) => (q, Vec::new()),
            QueryPlan::Hybrid {
                fts_query,
                short_terms,
            } => (fts_query, short_terms),
        };

        // FTS path: same trigram two-phase strategy as search_conversations.
        let scored = self.query_fts_rowids(&fts_query)?;

        if scored.is_empty() {
            let stats = self.gather_search_stats(filter, 0, false)?;
            return Ok(GroupedSearchResult {
                rows: Vec::new(),
                stats,
            });
        }

        // NOTE: no early stop here, unlike `search_conversations`. `match_count` and
        // `total_matched_messages` below are computed over ALL post-filter matches, so
        // every batch must be scanned. Adding an early break here silently corrupts them.
        const BATCH_SIZE: usize = 500;
        let mut all_results: Vec<SearchResultRow> = Vec::new();
        for chunk in scored.chunks(BATCH_SIZE) {
            let mut batch_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut sql = format!(
                "SELECT m.rowid AS message_rowid, m.message_uuid, m.session_id, m.parent_uuid, m.timestamp, m.message_type, \
                 m.project_path, m.depth, m.is_sidechain, m.full_content as context_snippet, \
                 c.conversation_summary, c.conversation_file, c.source \
                 FROM messages m JOIN conversations c ON m.session_id = c.session_id \
                 WHERE m.rowid IN ({}) AND m.is_meta_conversation = FALSE",
                placeholders
            );
            for (rowid, _) in chunk {
                batch_params.push(Box::new(*rowid));
            }
            // Sub-trigram terms narrow here; see search_conversations for the rationale.
            for term in &short_terms {
                sql.push_str(" AND m.full_content LIKE ? ESCAPE '\\'");
                batch_params.push(Box::new(format!("%{}%", escape_like(term))));
            }
            Self::append_filters(&mut sql, &mut batch_params, filter)?;
            // No ORDER BY: final order is decided in Rust below (see search_conversations).
            let batch_results = self.execute_search_typed(&sql, &batch_params)?;
            all_results.extend(batch_results);
        }

        // Full post-filter match count. This deliberately differs from
        // `search_conversations`, whose `matched_messages` counts RETURNED rows (truncated
        // to `limit`, and cut short by the early stop): grouping needs the true total to
        // report `match_count` per session.
        let total_matched_messages = all_results.len() as i64;

        // Group by session; count all matches. Sorting the flat list first means the
        // loop below picks up both the session order and its representative for free:
        // a session's score is the best (lowest) bm25 among its matches, and the first
        // row seen for a session is exactly that best-scoring message.
        //
        // The representative moves with the ranking on purpose. Surfacing a session
        // because it holds a highly relevant message, then showing a different message
        // as the snippet, makes the ranking look broken.
        let score_by_rowid: HashMap<i64, f64> = scored.iter().copied().collect();
        match filter.sort {
            SortOrder::Relevance => all_results.sort_by(|a, b| {
                let sa = score_by_rowid.get(&a.rowid).copied().unwrap_or(f64::MAX);
                let sb = score_by_rowid.get(&b.rowid).copied().unwrap_or(f64::MAX);
                sa.partial_cmp(&sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.timestamp.cmp(&a.timestamp))
            }),
            // Recent: representative reverts to the most recent matching message.
            SortOrder::Recent => all_results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
        }

        let search_terms: Vec<&str> = trimmed.split_whitespace().collect();
        let mut session_data: HashMap<String, (SearchResultRow, i64)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for mut row in all_results {
            let sid = row.session_id.clone();
            match session_data.entry(sid.clone()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    row.context_snippet = extract_snippet(&row.context_snippet, &search_terms, 200);
                    order.push(sid);
                    e.insert((row, 1));
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    e.get_mut().1 += 1;
                }
            }
        }
        // No early stop on this path, so every matching session is in `order` and the
        // comparison below is exact rather than a lower bound.
        let truncated = order.len() > limit_usize;
        order.truncate(limit_usize);
        let rows: Vec<GroupedRow> = order
            .into_iter()
            .map(|sid| {
                let (rep, count) = session_data.remove(&sid).expect(
                    "BUG: `order` must only contain session_ids inserted into session_data",
                );
                GroupedRow {
                    representative: rep,
                    match_count: count,
                }
            })
            .collect();

        let stats = self.gather_search_stats(filter, total_matched_messages, truncated)?;
        Ok(GroupedSearchResult { rows, stats })
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

    /// Returns `(rowid, bm25_score)` pairs sorted best-first.
    ///
    /// bm25 returns a NEGATIVE double where more negative means more relevant, so
    /// ascending order is best-first. Callers depend on this ordering for the
    /// early-stop optimization in `search_conversations`.
    fn query_fts_rowids(&self, fts_query: &str) -> Result<Vec<(i64, f64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT rowid, bm25(message_content_fts) AS score \
             FROM message_content_fts WHERE full_content MATCH ? \
             ORDER BY score",
        )?;
        let scored = stmt
            .query_map(rusqlite::params![fts_query], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<(i64, f64)>, _>>()?;
        Ok(scored)
    }

    fn execute_search_typed(
        &mut self,
        sql: &str,
        params: &[Box<dyn rusqlite::types::ToSql>],
    ) -> Result<Vec<SearchResultRow>> {
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

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

    /// Resolve a possibly-abbreviated session id to the full stored value.
    ///
    /// Exact match wins, so an id that also prefixes another one still resolves to
    /// itself. Failure is returned as the inner `Err(String)` rather than a hard
    /// error because callers surface it in-band via `ConversationTree::error`.
    fn resolve_session_id(&self, input: &str) -> Result<std::result::Result<String, String>> {
        let exact = self.query_rows(
            "SELECT session_id FROM conversations WHERE session_id = ?",
            &[&input as &dyn rusqlite::types::ToSql],
            |row| row.get::<_, String>(0),
        )?;
        if let Some(id) = exact.into_iter().next() {
            return Ok(Ok(id));
        }

        // OpenCode / Codex ids are stored with a source prefix (indexer/opencode.rs,
        // indexer/codex.rs), so a bare UUID copied from `search` output needs those
        // variants tried too. Anchored at the start only -- a leading `%` would let a
        // fragment match mid-UUID and silently pick an unrelated session.
        let esc = escape_like(input);
        let candidates = self.query_rows(
            "SELECT session_id FROM conversations
             WHERE session_id LIKE ?1 ESCAPE '\\'
                OR session_id LIKE ?2 ESCAPE '\\'
                OR session_id LIKE ?3 ESCAPE '\\'
             ORDER BY session_id
             LIMIT 11", // 10 to show a real count, plus one to detect "more than that"
            &[
                &format!("{}%", esc) as &dyn rusqlite::types::ToSql,
                &format!("oc:{}%", esc),
                &format!("codex:{}%", esc),
            ],
            |row| row.get::<_, String>(0),
        )?;

        match candidates.len() {
            0 => Ok(Err(format!("Conversation {} not found", input))),
            1 => Ok(Ok(candidates.into_iter().next().unwrap())),
            n => Ok(Err(format!(
                // `n` is capped by the LIMIT above, so past the cap report it as a floor
                // rather than telling the user a number that is simply wrong.
                "Ambiguous session id '{}' matches {}{} conversations: {}",
                input,
                n,
                if n >= 11 { "+" } else { "" },
                candidates
                    .iter()
                    .take(3)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    pub fn get_conversation_tree(&self, input_session_id: &str) -> Result<ConversationTree> {
        let session_id = match self.resolve_session_id(input_session_id)? {
            Ok(id) => id,
            Err(message) => {
                return Ok(ConversationTree {
                    conversation: None,
                    tree: Vec::new(),
                    total_messages: 0,
                    warning: None,
                    error: Some(message),
                })
            }
        };
        let session_id = session_id.as_str();

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
                warning: None,
                error: Some(format!("Conversation {} not found", input_session_id)),
            });
        }

        let conversation = conv.into_iter().next();
        if messages.is_empty() {
            return Self::get_conversation_tree_from_raw(session_id, conversation);
        }

        let tree = Self::build_tree(&messages);

        Ok(ConversationTree {
            conversation,
            tree,
            total_messages: messages.len(),
            warning: None,
            error: None,
        })
    }

    fn get_conversation_tree_from_raw(
        session_id: &str,
        conversation: Option<ConversationRow>,
    ) -> Result<ConversationTree> {
        let Some(conv) = conversation else {
            return Ok(ConversationTree {
                conversation: None,
                tree: Vec::new(),
                total_messages: 0,
                warning: None,
                error: Some(format!("Conversation {} not found", session_id)),
            });
        };

        let source = conv.source.as_deref().unwrap_or("claude_code").to_string();
        if source != "claude_code" {
            return Ok(ConversationTree {
                conversation: Some(conv),
                tree: Vec::new(),
                total_messages: 0,
                warning: None,
                error: Some(format!(
                    "Indexed messages are missing for this {} conversation; raw transcript fallback is only supported for Claude Code sessions",
                    source
                )),
            });
        }

        let Some(conversation_file) = conv.conversation_file.clone() else {
            return Ok(Self::raw_tree_error(
                conv,
                "Indexed messages are missing and no raw transcript path is recorded",
            ));
        };

        let path = std::path::Path::new(&conversation_file);
        let (_, raw_messages, skipped_lines) =
            match ConversationIndexer::parse_conversation_file_raw(path) {
                Ok(parsed) => parsed,
                Err(e) => {
                    return Ok(Self::raw_tree_error(
                        conv,
                        &format!(
                            "Indexed messages are missing and raw transcript could not be read at {}: {}",
                            conversation_file, e
                        ),
                    ));
                }
            };

        let total_raw = raw_messages.len();
        let messages: Vec<Message> = raw_messages
            .into_iter()
            .filter(|m| match m.session_id.as_deref() {
                Some(raw_session_id) => raw_session_id == session_id,
                None => true,
            })
            .collect();

        if messages.is_empty() {
            let reason = if total_raw == 0 && skipped_lines > 0 {
                format!(
                    "Indexed messages are missing and none of the {} non-empty line(s) in raw transcript at {} could be parsed; the file may be corrupt",
                    skipped_lines, conversation_file
                )
            } else {
                format!(
                    "Indexed messages are missing and raw transcript at {} contains no messages for session {} ({} message(s) in the file belong to other sessions)",
                    conversation_file, session_id, total_raw
                )
            };
            return Ok(Self::raw_tree_error(conv, &reason));
        }

        let tree =
            Self::build_tree_from_raw_messages(&messages, conv.project_path.as_deref(), session_id);

        let mut warning = if conv.message_count == 0 {
            // Resume handling can legitimately attribute all of a session's
            // messages to a sibling session; re-indexing will not change that.
            "Messages for this session are indexed under a sibling session; loaded tree from raw transcript.".to_string()
        } else {
            "Indexed messages are missing; loaded tree from raw transcript. Run ai-conversation-search index --all to repair the DB.".to_string()
        };
        if conv.message_count > 0 && (messages.len() as i64) < conv.message_count {
            warning.push_str(&format!(
                " Showing {} of {} recorded message(s).",
                messages.len(),
                conv.message_count
            ));
        }
        if skipped_lines > 0 {
            warning.push_str(&format!(
                " {} malformed line(s) were skipped; the transcript may be partially unreadable.",
                skipped_lines
            ));
        }

        Ok(ConversationTree {
            conversation: Some(conv),
            tree,
            total_messages: messages.len(),
            warning: Some(warning),
            error: None,
        })
    }

    fn raw_tree_error(conversation: ConversationRow, reason: &str) -> ConversationTree {
        // Re-indexing only helps when the index claims messages it doesn't have;
        // for message_count == 0 rows the state is reproduced by design.
        let error = if conversation.message_count > 0 {
            format!(
                "{}. Run ai-conversation-search index --all to repair the DB.",
                reason
            )
        } else {
            format!("{}.", reason)
        };
        ConversationTree {
            conversation: Some(conversation),
            tree: Vec::new(),
            total_messages: 0,
            warning: None,
            error: Some(error),
        }
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
                    children_map
                        .entry(parent_uuid.clone())
                        .or_default()
                        .push(msg.message_uuid.clone());
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
            let msg = msg_map
                .get(uuid)
                .expect("build_node called with uuid not in msg_map");
            let children = if let Some(kids) = children_map.get(uuid) {
                kids.iter()
                    .map(|k| build_node(k, msg_map, children_map))
                    .collect()
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
                // messages.summary is currently unpopulated by every indexer. Deriving
                // from full_content keeps this path aligned with the raw-transcript
                // fallback (build_tree_from_raw_messages) rather than rendering blank nodes.
                summary: msg
                    .summary
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| Some(summary_from_content(&msg.full_content))),
                full_content: msg.full_content.clone(),
                children,
            }
        }

        roots
            .iter()
            .map(|uuid| build_node(uuid, &msg_map, &children_map))
            .collect()
    }

    fn build_tree_from_raw_messages(
        messages: &[Message],
        project_path: Option<&str>,
        fallback_session_id: &str,
    ) -> Vec<TreeNode> {
        let depths = ConversationIndexer::calculate_depth(messages);
        let mut msg_map: HashMap<String, &Message> = HashMap::new();
        for msg in messages {
            msg_map.insert(msg.uuid.clone(), msg);
        }

        let mut roots = Vec::new();
        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();

        for msg in messages {
            if let Some(ref parent_uuid) = msg.parent_uuid {
                if msg_map.contains_key(parent_uuid) {
                    children_map
                        .entry(parent_uuid.clone())
                        .or_default()
                        .push(msg.uuid.clone());
                    continue;
                }
            }
            roots.push(msg.uuid.clone());
        }

        fn build_node(
            uuid: &str,
            msg_map: &HashMap<String, &Message>,
            children_map: &HashMap<String, Vec<String>>,
            depths: &HashMap<String, i32>,
            project_path: Option<&str>,
            fallback_session_id: &str,
        ) -> TreeNode {
            let msg = msg_map
                .get(uuid)
                .expect("build_node called with uuid not in msg_map");
            let children = if let Some(kids) = children_map.get(uuid) {
                kids.iter()
                    .map(|k| {
                        build_node(
                            k,
                            msg_map,
                            children_map,
                            depths,
                            project_path,
                            fallback_session_id,
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            TreeNode {
                message_uuid: msg.uuid.clone(),
                session_id: msg
                    .session_id
                    .clone()
                    .unwrap_or_else(|| fallback_session_id.to_string()),
                parent_uuid: msg.parent_uuid.clone(),
                is_sidechain: msg.is_sidechain,
                depth: i64::from(*depths.get(&msg.uuid).unwrap_or(&0)),
                timestamp: msg.timestamp.clone().unwrap_or_default(),
                message_type: msg.message_type.clone(),
                project_path: project_path.map(str::to_string),
                summary: Some(summary_from_content(&msg.content)),
                full_content: msg.content.clone(),
                children,
            }
        }

        roots
            .iter()
            .map(|uuid| {
                build_node(
                    uuid,
                    &msg_map,
                    &children_map,
                    &depths,
                    project_path,
                    fallback_session_id,
                )
            })
            .collect()
    }

    pub fn list_recent_conversations(&self, filter: &SearchFilter<'_>) -> Result<ListResult> {
        let days_back = filter.days_back;
        let since = filter.since;
        let until = filter.until;
        let date = filter.date;
        let limit = filter.limit;
        let project_path = filter.project_path;
        let repo = filter.repo;
        let source = filter.source;

        let effective_days =
            if days_back.is_none() && since.is_none() && until.is_none() && date.is_none() {
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

        // Rejected rather than clamped, matching search_conversations: a negative limit
        // means one thing to SQL (unlimited, by accident) and another to `truncate`.
        if limit < 0 {
            return Err(AppError::General(format!(
                "limit must be >= 0, got {}",
                limit
            )));
        }

        // Over-fetch by one. SQL caps the set, so the extra row is the only thing that
        // separates "exactly `limit` conversations exist" from "more were cut off".
        sql.push_str(" ORDER BY last_message_at DESC LIMIT ?");
        params.push(Box::new(limit.saturating_add(1)));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        // Known limitation, shared with the search paths: query_rows drops rows whose
        // mapping fails with a log::warn, so a failure on the over-fetched row would
        // under-report truncation rather than over-report it.
        let mut rows = self.query_rows(&sql, &param_refs, ConversationRow::from_row)?;
        let truncated = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        Ok(ListResult { rows, truncated })
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

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let conversations = self.query_rows(&sql, &param_refs, ConversationRow::from_row)?;

        if conversations.is_empty() {
            return Ok(format!(
                "No conversations found in the last {} day(s).",
                days_back
            ));
        }

        let day_word = if days_back == 1 { "" } else { "s" };
        let mut lines = vec![format!(
            "# Conversations (last {} day{})\n",
            days_back, day_word
        )];

        for conv in &conversations {
            let session_id = &conv.session_id;
            let summary = conv.conversation_summary.as_deref().unwrap_or("");
            let project = conv.project_path.as_deref().unwrap_or("");
            let msg_count = conv.message_count;
            let last_at = conv.last_message_at.as_deref().unwrap_or("");

            let date_str = format_timestamp(last_at, true, false);
            let session_short = &session_id[..std::cmp::min(8, session_id.len())];

            lines.push(format!("## [{}] {}", session_short, summary));
            lines.push(format!(
                "**{} msgs** | {} | {}\n",
                msg_count, project, date_str
            ));

            // Fetch messages
            let msg_sql = "SELECT message_uuid, timestamp, message_type, summary, is_sidechain, project_path, is_tool_noise FROM messages WHERE session_id = ? AND is_tool_noise = FALSE AND is_meta_conversation = FALSE ORDER BY timestamp DESC LIMIT ?";
            let mut msg_results = self.query_rows(
                msg_sql,
                &[
                    session_id as &dyn rusqlite::types::ToSql,
                    &max_messages_per_conv as &dyn rusqlite::types::ToSql,
                ],
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
                let icon = if msg_type == "user" {
                    "\u{1f464}"
                } else {
                    "\u{1f916}"
                };
                let branch = if *is_sidechain { "\u{1f33f} " } else { "" };
                let uuid_short = &uuid[..8.min(uuid.len())];

                lines.push(format!(
                    "{} {} `{}` {}{}",
                    icon, msg_time, uuid_short, branch, msg_summary
                ));
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
    use std::io::Write;

    fn default_filter() -> SearchFilter<'static> {
        SearchFilter {
            limit: 10,
            ..Default::default()
        }
    }

    /// The early-stop in the FTS path must never drop rows that filters would have kept.
    ///
    /// This is the guard against the tempting-but-wrong optimization of truncating the
    /// phase-1 rowid list before phase 2: filters are applied only in phase 2, so if the
    /// best-scoring hits all belong to another project, truncating early returns nothing.
    /// The fixture puts >500 (one full batch) high-scoring rows in /projB and the only
    /// /projA rows at the very end of the score order.
    #[test]
    fn test_early_stop_respects_filters() {
        let conn = setup_test_db();
        for (sid, proj) in [("sessA", "/projA"), ("sessB", "/projB")] {
            insert_test_conversation(
                &conn,
                sid,
                proj,
                "summary",
                "2025-01-15T09:00:00",
                "2025-01-15T10:00:00",
                "claude_code",
            );
        }

        // 600 tight (high-scoring) matches in the filtered-out project: more than one
        // BATCH_SIZE of 500, so the batching loop genuinely engages.
        for i in 0..600 {
            insert_test_message(
                &conn,
                &format!("b{}", i),
                "sessB",
                "rustacean",
                "user",
                "2025-01-15T10:00:00",
                "/projB",
            );
        }
        // The surviving rows score worst, so they land in the LAST batch.
        let padded = format!(
            "{} rustacean {}",
            "filler ".repeat(300),
            "filler ".repeat(300)
        );
        for i in 0..2 {
            insert_test_message(
                &conn,
                &format!("a{}", i),
                "sessA",
                &padded,
                "user",
                "2025-01-15T10:00:00",
                "/projA",
            );
        }

        let mut searcher = ConversationSearch::from_connection(conn);
        let filter = SearchFilter {
            limit: 20,
            project_path: Some("/projA"),
            ..Default::default()
        };
        let rows = searcher
            .search_conversations("rustacean", &filter)
            .unwrap()
            .rows;

        assert_eq!(
            rows.len(),
            2,
            "early stop must not discard rows the filter would keep"
        );
        assert!(rows.iter().all(|r| r.session_id == "sessA"));
    }

    /// Pins the SQL-side `ORDER BY score` direction in `query_fts_rowids`, which the Rust
    /// comparator cannot cover: on a single-batch fixture the phase-1 order is invisible
    /// because the final order is recomputed in Rust.
    ///
    /// It only becomes observable once the early stop fires. With phase 1 sorted
    /// worst-first, the stop fills `all_results` from the WORST candidates and breaks
    /// before ever reaching the good one, which then vanishes from the results entirely.
    #[test]
    fn test_phase1_order_is_best_first() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );

        // One tight match buried among 700 padded ones: more than one BATCH_SIZE, so the
        // early stop fires long before the last batch.
        let padded = format!(
            "{} rustacean {}",
            "filler ".repeat(300),
            "filler ".repeat(300)
        );
        for i in 0..700 {
            insert_test_message(
                &conn,
                &format!("pad{}", i),
                "sess1",
                &padded,
                "user",
                "2025-01-15T10:00:00",
                "/proj",
            );
        }
        insert_test_message(
            &conn,
            "best",
            "sess1",
            "rustacean",
            "user",
            "2025-01-15T09:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let filter = SearchFilter {
            limit: 5,
            ..Default::default()
        };
        let rows = searcher
            .search_conversations("rustacean", &filter)
            .unwrap()
            .rows;

        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0].message_uuid, "best",
            "the best match must survive the early stop"
        );
    }

    /// `search_conversations` does not validate `limit` (unlike the grouped path), so the
    /// degenerate values must at least not panic.
    #[test]
    fn test_search_zero_limit_returns_empty() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "rustacean content",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);

        let zero = SearchFilter {
            limit: 0,
            ..Default::default()
        };
        assert_eq!(
            searcher
                .search_conversations("rustacean", &zero)
                .unwrap()
                .rows
                .len(),
            0
        );

        // A negative limit is an input error, not "unlimited": rejected on both the
        // grouped and non-grouped paths.
        let negative = SearchFilter {
            limit: -1,
            ..Default::default()
        };
        let err = searcher
            .search_conversations("rustacean", &negative)
            .expect_err("negative limit must be rejected");
        assert!(
            err.to_string().contains("limit must be >= 0"),
            "unexpected error: {}",
            err
        );
    }

    // ---- truncation reporting tests ----

    /// Inserts `count` matching messages, one session each, so session-level and
    /// message-level limits can both be exercised.
    fn insert_matching_messages(conn: &Connection, count: usize, content: &str) {
        for i in 0..count {
            let sess = format!("sess{}", i);
            insert_test_conversation(
                conn,
                &sess,
                "/proj",
                "summary",
                "2025-01-15T09:00:00",
                "2025-01-15T10:00:00",
                "claude_code",
            );
            insert_test_message(
                conn,
                &format!("msg{}", i),
                &sess,
                content,
                "user",
                &format!("2025-01-15T10:0{}:00", i),
                "/proj",
            );
        }
    }

    #[test]
    fn test_grouped_search_negative_limit_is_rejected() {
        let conn = setup_test_db();
        insert_matching_messages(&conn, 2, "rustacean content");
        let mut searcher = ConversationSearch::from_connection(conn);

        let negative = SearchFilter {
            limit: -1,
            ..Default::default()
        };
        let err = searcher
            .search_grouped_by_session("rustacean", &negative)
            .expect_err("negative limit must be rejected");
        assert!(
            err.to_string().contains("limit must be >= 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_search_reports_truncation_when_more_results_exist() {
        let conn = setup_test_db();
        insert_matching_messages(&conn, 5, "rustacean content");
        let mut searcher = ConversationSearch::from_connection(conn);

        let filter = SearchFilter {
            limit: 3,
            ..Default::default()
        };
        let result = searcher.search_conversations("rustacean", &filter).unwrap();

        assert_eq!(result.rows.len(), 3);
        assert!(result.stats.truncated);
    }

    #[test]
    fn test_search_no_truncation_flag_when_within_limit() {
        let conn = setup_test_db();
        insert_matching_messages(&conn, 3, "rustacean content");
        let mut searcher = ConversationSearch::from_connection(conn);

        let filter = SearchFilter {
            limit: 10,
            ..Default::default()
        };
        let result = searcher.search_conversations("rustacean", &filter).unwrap();

        assert_eq!(result.rows.len(), 3);
        assert!(!result.stats.truncated);
    }

    #[test]
    fn test_search_truncation_boundary_exact_limit() {
        let conn = setup_test_db();
        insert_matching_messages(&conn, 3, "rustacean content");
        let mut searcher = ConversationSearch::from_connection(conn);

        // Matches == limit and every candidate was scanned, so nothing was dropped.
        let filter = SearchFilter {
            limit: 3,
            ..Default::default()
        };
        let result = searcher.search_conversations("rustacean", &filter).unwrap();

        assert_eq!(result.rows.len(), 3);
        assert!(!result.stats.truncated);
    }

    /// Matches == limit means nothing was dropped. Without this the notice would fire on
    /// every query that happens to land exactly on the cap, training callers to ignore it.
    #[test]
    fn test_truncation_boundary_exact_limit_all_paths() {
        let conn = setup_test_db();
        insert_matching_messages(&conn, 3, "ab rustacean content");
        let mut searcher = ConversationSearch::from_connection(conn);

        let filter = SearchFilter {
            limit: 3,
            ..Default::default()
        };

        for (label, truncated) in [
            (
                "fts grouped",
                searcher
                    .search_grouped_by_session("rustacean", &filter)
                    .unwrap()
                    .stats
                    .truncated,
            ),
            (
                "like-only",
                searcher
                    .search_conversations("ab", &filter)
                    .unwrap()
                    .stats
                    .truncated,
            ),
            (
                "like-only grouped",
                searcher
                    .search_grouped_by_session("ab", &filter)
                    .unwrap()
                    .stats
                    .truncated,
            ),
        ] {
            assert!(!truncated, "{} falsely reported truncation", label);
        }
    }

    #[test]
    fn test_grouped_search_reports_truncation() {
        let conn = setup_test_db();
        insert_matching_messages(&conn, 5, "rustacean content");
        let mut searcher = ConversationSearch::from_connection(conn);

        let filter = SearchFilter {
            limit: 2,
            ..Default::default()
        };
        let result = searcher
            .search_grouped_by_session("rustacean", &filter)
            .unwrap();

        assert_eq!(result.rows.len(), 2);
        assert!(result.stats.truncated);
    }

    /// All-short queries bypass FTS and cap in SQL, so truncation has to be detected by
    /// over-fetching rather than by inspecting a collected vector.
    #[test]
    fn test_like_only_search_reports_truncation() {
        let conn = setup_test_db();
        insert_matching_messages(&conn, 5, "ab cd content");
        let mut searcher = ConversationSearch::from_connection(conn);

        let filter = SearchFilter {
            limit: 2,
            ..Default::default()
        };
        let result = searcher.search_conversations("ab", &filter).unwrap();
        assert_eq!(result.rows.len(), 2);
        assert!(result.stats.truncated);

        let grouped = searcher.search_grouped_by_session("ab", &filter).unwrap();
        assert_eq!(grouped.rows.len(), 2);
        assert!(grouped.stats.truncated);
    }

    /// When NO term reaches 3 characters the query keeps AND semantics and recency
    /// ordering, while the FTS path uses OR + bm25. That asymmetry is deliberate: there is
    /// no ranking signal here, and a 2-char term ORed against anything matches nearly
    /// everything.
    #[test]
    fn test_like_fallback_still_and_and_recency_ordered() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "both_old",
            "sess1",
            "認証 実装 の話",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "both_new",
            "sess1",
            "実装 と 認証 について",
            "user",
            "2025-01-15T11:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "one_only",
            "sess1",
            "認証 だけ",
            "user",
            "2025-01-15T12:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let rows = searcher
            .search_conversations("認証 実装", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(rows.len(), 2, "AND semantics: one_only must not match");
        assert_eq!(rows[0].message_uuid, "both_new", "recency order");
        assert_eq!(rows[1].message_uuid, "both_old");
    }

    /// bm25 is negative and lower is better. If the ORDER BY sign were inverted, the
    /// long sparse match would come first.
    #[test]
    fn test_bm25_sign_convention_lower_is_better() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "dense",
            "sess1",
            "rustacean rustacean",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        let sparse = format!(
            "{} rustacean {}",
            "filler ".repeat(300),
            "filler ".repeat(300)
        );
        insert_test_message(
            &conn,
            "sparse",
            "sess1",
            &sparse,
            "user",
            "2025-01-15T10:01:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let rows = searcher
            .search_conversations("rustacean", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].message_uuid, "dense");
    }

    /// The reason this feature exists: an older, highly relevant message must outrank a
    /// newer, barely relevant one.
    #[test]
    fn test_bm25_ranking_beats_recency() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-20T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "old_relevant",
            "sess1",
            "rustacean rustacean rustacean",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        let new_sparse = format!(
            "{} rustacean {}",
            "filler ".repeat(300),
            "filler ".repeat(300)
        );
        insert_test_message(
            &conn,
            "new_sparse",
            "sess1",
            &new_sparse,
            "user",
            "2025-01-20T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let rows = searcher
            .search_conversations("rustacean", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].message_uuid, "old_relevant",
            "relevance must beat recency by default"
        );

        // ...and --sort=recent restores the old behavior.
        let recent_filter = SearchFilter {
            limit: 10,
            sort: SortOrder::Recent,
            ..Default::default()
        };
        let rows = searcher
            .search_conversations("rustacean", &recent_filter)
            .unwrap()
            .rows;
        assert_eq!(rows[0].message_uuid, "new_sparse");
    }

    /// Scores come from phase 1 but rows are filtered in phase 2. The rowid join must
    /// survive filters removing rows.
    #[test]
    fn test_rowid_join_survives_filters() {
        let conn = setup_test_db();
        for (sid, proj) in [("sessA", "/projA"), ("sessB", "/projB")] {
            insert_test_conversation(
                &conn,
                sid,
                proj,
                "summary",
                "2025-01-15T09:00:00",
                "2025-01-15T10:00:00",
                "claude_code",
            );
        }
        let sparse = format!(
            "{} rustacean {}",
            "filler ".repeat(300),
            "filler ".repeat(300)
        );
        insert_test_message(
            &conn,
            "a_sparse",
            "sessA",
            &sparse,
            "user",
            "2025-01-15T10:00:00",
            "/projA",
        );
        insert_test_message(
            &conn,
            "a_dense",
            "sessA",
            "rustacean rustacean",
            "user",
            "2025-01-15T10:01:00",
            "/projA",
        );
        insert_test_message(
            &conn,
            "b_dense",
            "sessB",
            "rustacean rustacean",
            "user",
            "2025-01-15T10:02:00",
            "/projB",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let filter = SearchFilter {
            limit: 10,
            project_path: Some("/projA"),
            ..Default::default()
        };
        let rows = searcher
            .search_conversations("rustacean", &filter)
            .unwrap()
            .rows;

        assert_eq!(rows.len(), 2, "only /projA rows survive the filter");
        assert!(rows.iter().all(|r| r.session_id == "sessA"));
        assert_eq!(rows[0].message_uuid, "a_dense", "still score-ordered");
    }

    /// The `rowid` field is an internal join key for bm25 scores. `--json` output is
    /// consumed by bin/ai-conversation-search, so it must not leak into the payload.
    #[test]
    fn test_json_output_has_no_rowid_field() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "rustacean content",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let rows = searcher
            .search_conversations("rustacean", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(rows.len(), 1);
        let json = serde_json::to_string(&rows[0]).unwrap();
        assert!(
            !json.contains("rowid"),
            "rowid must be skipped in JSON output, got: {}",
            json
        );
    }

    /// Seed one conversation plus the given `(uuid, content, timestamp)` messages.
    fn setup_hybrid_db(messages: &[(&str, &str, &str)]) -> ConversationSearch {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-01T00:00:00",
            "2025-01-31T00:00:00",
            "claude_code",
        );
        for (uuid, content, ts) in messages {
            insert_test_message(&conn, uuid, "sess1", content, "user", ts, "/proj");
        }
        ConversationSearch::from_connection(conn)
    }

    fn uuids(rows: &[SearchResultRow]) -> Vec<&str> {
        rows.iter().map(|r| r.message_uuid.as_str()).collect()
    }

    /// A term under the trigram minimum cannot be ranked by, so it acts as a mandatory
    /// filter on the FTS candidates rather than being silently dropped.
    #[test]
    fn test_hybrid_short_term_narrows_fts_results() {
        let mut s = setup_hybrid_db(&[
            (
                "msg1",
                "デプロイの手順をまとめました",
                "2025-01-10T10:00:00",
            ),
            (
                "msg2",
                "デプロイが失敗した原因を調べる",
                "2025-01-11T10:00:00",
            ),
        ]);

        let rows = s
            .search_conversations("デプロイ 失敗", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(uuids(&rows), vec!["msg2"]);
    }

    /// The point of routing mixed queries through FTS: bm25 outranks recency, so a long
    /// machine-generated message cannot bury a tight match just by being newer.
    #[test]
    fn test_hybrid_ranks_by_bm25_not_recency() {
        let padded = format!("デプロイ 失敗 {}", "filler ".repeat(400));
        let mut s = setup_hybrid_db(&[
            ("old_tight", "デプロイ 失敗", "2025-01-02T10:00:00"),
            ("new_padded", &padded, "2025-01-30T10:00:00"),
        ]);

        let rows = s
            .search_conversations("デプロイ 失敗", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(uuids(&rows), vec!["old_tight", "new_padded"]);
    }

    /// When nothing reaches the trigram minimum there is no ranking signal at all, so the
    /// query keeps AND semantics and recency order.
    #[test]
    fn test_hybrid_all_short_terms_falls_back_to_like() {
        let mut s = setup_hybrid_db(&[
            ("only_one", "認証だけの話", "2025-01-20T10:00:00"),
            ("both_old", "認証と実装の話", "2025-01-05T10:00:00"),
            ("both_new", "実装した認証", "2025-01-25T10:00:00"),
        ]);

        let rows = s
            .search_conversations("認証 実装", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(uuids(&rows), vec!["both_new", "both_old"]);
    }

    /// `OR` is two characters, so a length-only short-term check used to misclassify
    /// operator queries and match `%OR%` as a literal instead of reaching FTS.
    #[test]
    fn test_or_operator_reaches_fts() {
        let mut s = setup_hybrid_db(&[
            ("msg1", "rustacean content", "2025-01-10T10:00:00"),
            ("msg2", "kubernetes content", "2025-01-11T10:00:00"),
            ("msg3", "unrelated content", "2025-01-12T10:00:00"),
        ]);

        let rows = s
            .search_conversations("rustacean OR kubernetes", &default_filter())
            .unwrap()
            .rows;

        let mut got = uuids(&rows);
        got.sort_unstable();
        assert_eq!(got, vec!["msg1", "msg2"]);
    }

    /// All-short operator queries must stay on the LIKE path: handed to FTS verbatim they
    /// tokenize to nothing and return zero rows.
    #[test]
    fn test_all_short_operator_query_stays_on_like_path() {
        let mut s = setup_hybrid_db(&[("msg1", "更新 OR 認証 をまとめた", "2025-01-10T10:00:00")]);

        let rows = s
            .search_conversations("更新 OR 認証", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(uuids(&rows), vec!["msg1"]);
    }

    /// A sub-trigram operand inside an explicit-operator query cannot reach FTS: it would
    /// tokenize to nothing and quietly rewrite the operator (`AND` matching zero rows,
    /// `NOT` excluding nothing). Such queries stay on the LIKE path instead.
    #[test]
    fn test_operator_query_with_short_operand_stays_on_like_path() {
        let mut s = setup_hybrid_db(&[
            ("both", "デプロイ AND 失敗 の記録", "2025-01-10T10:00:00"),
            ("only_long", "デプロイ AND だけ", "2025-01-11T10:00:00"),
        ]);

        let rows = s
            .search_conversations("デプロイ AND 失敗", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(uuids(&rows), vec!["both"]);
    }

    /// The short-term narrowing lives in the phase-2 query, so it must apply under
    /// `SortOrder::Recent` too -- where the early stop is disabled and every batch is read.
    #[test]
    fn test_hybrid_short_term_narrows_under_sort_recent() {
        let mut s = setup_hybrid_db(&[
            ("no_short", "デプロイの手順", "2025-01-20T10:00:00"),
            ("has_short", "デプロイが失敗した", "2025-01-10T10:00:00"),
        ]);

        let rows = s
            .search_conversations(
                "デプロイ 失敗",
                &SearchFilter {
                    limit: 10,
                    sort: SortOrder::Recent,
                    ..Default::default()
                },
            )
            .unwrap()
            .rows;

        assert_eq!(uuids(&rows), vec!["has_short"]);
    }

    /// The short-term LIKE and `append_filters` push parameters into the same statement;
    /// this pins that their placeholders stay in step.
    #[test]
    fn test_hybrid_short_term_coexists_with_project_filter() {
        let conn = setup_test_db();
        for (sess, proj) in [("sessA", "/projA"), ("sessB", "/projB")] {
            insert_test_conversation(
                &conn,
                sess,
                proj,
                "summary",
                "2025-01-01T00:00:00",
                "2025-01-31T00:00:00",
                "claude_code",
            );
            insert_test_message(
                &conn,
                &format!("{}_msg", sess),
                sess,
                "デプロイが失敗した",
                "user",
                "2025-01-10T10:00:00",
                proj,
            );
        }

        let mut s = ConversationSearch::from_connection(conn);
        let rows = s
            .search_conversations(
                "デプロイ 失敗",
                &SearchFilter {
                    limit: 10,
                    project_path: Some("/projA"),
                    ..Default::default()
                },
            )
            .unwrap()
            .rows;

        assert_eq!(uuids(&rows), vec!["sessA_msg"]);
    }

    /// The early stop must not fire on unfiltered candidates. The filler outranks the
    /// survivors on bm25 (it is dense in the long term), so the survivors land in a later
    /// batch and are only reached if the short-term LIKE is applied per batch.
    #[test]
    fn test_hybrid_early_stop_respects_short_term_filter() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-01T00:00:00",
            "2025-01-31T00:00:00",
            "claude_code",
        );
        for i in 0..600 {
            insert_test_message(
                &conn,
                &format!("filler{:04}", i),
                "sess1",
                "デプロイ",
                "user",
                "2025-01-02T10:00:00",
                "/proj",
            );
        }
        // Padded so bm25 ranks all three below every filler row, which puts them past the
        // first batch. Three of them against limit=2 makes the early stop actually fire.
        for (uuid, pad) in [("surv_a", 20), ("surv_b", 60), ("surv_c", 120)] {
            let content = format!("デプロイ 失敗 {}", "noise ".repeat(pad));
            insert_test_message(
                &conn,
                uuid,
                "sess1",
                &content,
                "user",
                "2025-01-03T10:00:00",
                "/proj",
            );
        }

        let mut s = ConversationSearch::from_connection(conn);
        let rows = s
            .search_conversations(
                "デプロイ 失敗",
                &SearchFilter {
                    limit: 2,
                    ..Default::default()
                },
            )
            .unwrap()
            .rows;

        assert_eq!(uuids(&rows), vec!["surv_a", "surv_b"]);
    }

    /// The grouped path applies the same narrowing, so `match_count` reflects the rows that
    /// actually contain every term -- not the wider FTS candidate set.
    #[test]
    fn test_grouped_hybrid_match_count() {
        let mut s = setup_hybrid_db(&[
            ("msg1", "デプロイ 失敗 その1", "2025-01-10T10:00:00"),
            ("msg2", "デプロイ 失敗 その2", "2025-01-11T10:00:00"),
            ("msg3", "デプロイ だけ", "2025-01-12T10:00:00"),
        ]);

        let rows = s
            .search_grouped_by_session("デプロイ 失敗", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].match_count, 2);
    }

    /// Short terms are excluded from matching but not from highlighting.
    #[test]
    fn test_hybrid_snippet_highlights_short_term() {
        let mut s = setup_hybrid_db(&[("msg1", "失敗 したデプロイの記録", "2025-01-10T10:00:00")]);

        let rows = s
            .search_conversations("デプロイ 失敗", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].context_snippet.contains("**失敗**"),
            "short term must still be highlighted, got: {}",
            rows[0].context_snippet
        );
    }

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(include_str!("../data/schema.sql"))
            .unwrap();
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
        insert_test_message_full(
            conn,
            uuid,
            session_id,
            content,
            msg_type,
            timestamp,
            project_path,
            false,
            false,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_test_message_full(
        conn: &Connection,
        uuid: &str,
        session_id: &str,
        content: &str,
        msg_type: &str,
        timestamp: &str,
        project_path: &str,
        is_meta_conversation: bool,
        is_sidechain: bool,
    ) {
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, is_meta_conversation, is_tool_noise) VALUES (?, ?, NULL, ?, 0, ?, ?, ?, 'test.jsonl', ?, ?, FALSE)",
            rusqlite::params![uuid, session_id, is_sidechain, timestamp, msg_type, project_path, content, is_meta_conversation],
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

    fn write_test_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
        file
    }

    /// Insert a conversations row with no matching messages rows.
    fn insert_orphan_conversation(
        conn: &Connection,
        session_id: &str,
        conversation_file: Option<&str>,
        message_count: i64,
        source: &str,
    ) {
        conn.execute(
            "INSERT INTO conversations (session_id, project_path, conversation_file, root_message_uuid, conversation_summary, first_message_at, last_message_at, message_count, source) VALUES (?, '/proj', ?, 'root1', 'orphan', '2025-01-15T10:00:00', '2025-01-15T10:01:00', ?, ?)",
            rusqlite::params![session_id, conversation_file, message_count, source],
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
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "hello world",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess1",
            "goodbye world",
            "assistant",
            "2025-01-15T11:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(results.len(), 2);
        // Should be ordered by timestamp DESC
        assert!(results[0].timestamp >= results[1].timestamp);
    }

    #[test]
    fn test_search_fts_match() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "the quick brown fox jumps over the lazy dog",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess1",
            "rust programming language is great",
            "assistant",
            "2025-01-15T11:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("fox", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_no_results() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "hello world",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("xyzzyzzy", &default_filter())
            .unwrap()
            .rows;

        assert!(results.is_empty());
    }

    #[test]
    fn test_search_days_back_filter() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2020-01-01T00:00:00",
            "2020-01-01T01:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "old_msg",
            "sess1",
            "very old message content",
            "user",
            "2020-01-01T00:00:00",
            "/proj",
        );

        insert_test_conversation(
            &conn,
            "sess2",
            "/proj",
            "summary2",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        // Use a timestamp that's definitely recent
        let recent_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        insert_test_message(
            &conn,
            "new_msg",
            "sess2",
            "brand new message content",
            "user",
            &recent_ts,
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations(
                "message content",
                &SearchFilter {
                    days_back: Some(1),
                    ..default_filter()
                },
            )
            .unwrap()
            .rows;

        // Only the recent message should be returned
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "new_msg");
    }

    #[test]
    fn test_search_project_filter() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj_a",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sess2",
            "/proj_b",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "hello from project a",
            "user",
            "2025-01-15T10:00:00",
            "/proj_a",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess2",
            "hello from project b",
            "user",
            "2025-01-15T10:00:00",
            "/proj_b",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations(
                "hello",
                &SearchFilter {
                    project_path: Some("/proj_a"),
                    ..default_filter()
                },
            )
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].project_path.as_deref(), Some("/proj_a"));
    }

    #[test]
    fn test_search_source_filter() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sess2",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "opencode",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "message from claude",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess2",
            "message from opencode",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations(
                "message",
                &SearchFilter {
                    source: Some("opencode"),
                    ..default_filter()
                },
            )
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source.as_deref(), Some("opencode"));
    }

    #[test]
    fn test_search_repo_filter() {
        let conn = setup_test_db();
        insert_test_conversation_with_repo(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
            "/home/user/my-repo",
        );
        insert_test_conversation_with_repo(
            &conn,
            "sess2",
            "/proj2",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
            "/home/user/other-repo",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "code in my repo",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess2",
            "code in other repo",
            "user",
            "2025-01-15T10:00:00",
            "/proj2",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations(
                "code",
                &SearchFilter {
                    repo: Some("my-repo"),
                    ..default_filter()
                },
            )
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_date_filter() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-14T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "message on jan 15",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess1",
            "message on jan 14",
            "user",
            "2025-01-14T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations(
                "message",
                &SearchFilter {
                    date: Some("2025-01-15"),
                    ..default_filter()
                },
            )
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_conflicting_filters() {
        let conn = setup_test_db();
        let mut searcher = ConversationSearch::from_connection(conn);

        let result = searcher.search_conversations(
            "test",
            &SearchFilter {
                days_back: Some(7),
                since: Some("2025-01-01"),
                ..default_filter()
            },
        );

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Cannot use --days"));
    }

    // ---- search_grouped_by_session tests ----

    /// Empty query: limit applies at SESSION level. 3 sessions exist (10 + 10 + 5
    /// messages); with limit=2 we should get 2 sessions, not 2 messages from one
    /// session. match_count is the per-session message total.
    #[test]
    fn test_search_grouped_empty_query_limits_sessions() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sessA",
            "/proj",
            "session A",
            "2025-01-15T09:00:00",
            "2025-01-15T19:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sessB",
            "/proj",
            "session B",
            "2025-01-15T09:00:00",
            "2025-01-15T18:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sessC",
            "/proj",
            "session C",
            "2025-01-15T09:00:00",
            "2025-01-15T17:00:00",
            "claude_code",
        );
        for i in 0..10 {
            insert_test_message(
                &conn,
                &format!("a{}", i),
                "sessA",
                "content A",
                "user",
                &format!("2025-01-15T1{}:00:00", i),
                "/proj",
            );
        }
        for i in 0..10 {
            insert_test_message(
                &conn,
                &format!("b{}", i),
                "sessB",
                "content B",
                "user",
                &format!("2025-01-14T1{}:00:00", i),
                "/proj",
            );
        }
        for i in 0..5 {
            insert_test_message(
                &conn,
                &format!("c{}", i),
                "sessC",
                "content C",
                "user",
                &format!("2025-01-13T1{}:00:00", i),
                "/proj",
            );
        }

        let mut searcher = ConversationSearch::from_connection(conn);
        let result = searcher
            .search_grouped_by_session(
                "",
                &SearchFilter {
                    limit: 2,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(result.rows.len(), 2, "limit must apply at session level");
        assert_eq!(result.rows[0].representative.session_id, "sessA");
        assert_eq!(result.rows[1].representative.session_id, "sessB");
        assert_eq!(result.rows[0].match_count, 10);
        assert_eq!(result.rows[1].match_count, 10);
    }

    /// FTS path also limits at session level. "common" appears in 3 sessions;
    /// limit=2 returns 2 sessions ordered by their representative timestamp.
    #[test]
    fn test_search_grouped_fts_limits_sessions() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sessA",
            "/proj",
            "A",
            "2025-01-15T09:00:00",
            "2025-01-15T19:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sessB",
            "/proj",
            "B",
            "2025-01-15T09:00:00",
            "2025-01-15T18:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sessC",
            "/proj",
            "C",
            "2025-01-15T09:00:00",
            "2025-01-15T17:00:00",
            "claude_code",
        );
        // sessA: 3 matches; sessB: 2 matches; sessC: 1 match
        for i in 0..3 {
            insert_test_message(
                &conn,
                &format!("a{}", i),
                "sessA",
                "common keyword here",
                "user",
                &format!("2025-01-15T1{}:00:00", i),
                "/proj",
            );
        }
        for i in 0..2 {
            insert_test_message(
                &conn,
                &format!("b{}", i),
                "sessB",
                "common keyword here",
                "user",
                &format!("2025-01-14T1{}:00:00", i),
                "/proj",
            );
        }
        insert_test_message(
            &conn,
            "c0",
            "sessC",
            "common keyword here",
            "user",
            "2025-01-13T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let result = searcher
            .search_grouped_by_session(
                "common",
                &SearchFilter {
                    limit: 2,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].representative.session_id, "sessA");
        assert_eq!(result.rows[0].match_count, 3);
        assert_eq!(result.rows[1].representative.session_id, "sessB");
        assert_eq!(result.rows[1].match_count, 2);
    }

    /// An all-short query (no term reaching 3 codepoints) goes through the LIKE-only path,
    /// which also uses the window-function session-level grouping.
    #[test]
    fn test_search_grouped_short_query_like_path() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sessA",
            "/proj",
            "A",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sessB",
            "/proj",
            "B",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "a1",
            "sessA",
            "ok done",
            "user",
            "2025-01-15T11:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "a2",
            "sessA",
            "ok again",
            "user",
            "2025-01-15T10:30:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "b1",
            "sessB",
            "ok one",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let result = searcher
            .search_grouped_by_session(
                "ok",
                &SearchFilter {
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].representative.session_id, "sessA");
        assert_eq!(result.rows[0].match_count, 2);
        assert_eq!(result.rows[1].representative.session_id, "sessB");
        assert_eq!(result.rows[1].match_count, 1);
    }

    /// Filters (project, source, repo) compose with session-level limit.
    #[test]
    fn test_search_grouped_preserves_filters() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sessA",
            "/proj_a",
            "A",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sessB",
            "/proj_b",
            "B",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "opencode",
        );
        insert_test_message(
            &conn,
            "a1",
            "sessA",
            "topic alpha",
            "user",
            "2025-01-15T11:00:00",
            "/proj_a",
        );
        insert_test_message(
            &conn,
            "b1",
            "sessB",
            "topic beta",
            "user",
            "2025-01-15T10:00:00",
            "/proj_b",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        // FTS path with source filter
        let result = searcher
            .search_grouped_by_session(
                "topic",
                &SearchFilter {
                    source: Some("opencode"),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].representative.session_id, "sessB");

        // Empty-query window path with project filter
        let result = searcher
            .search_grouped_by_session(
                "",
                &SearchFilter {
                    project_path: Some("/proj_a"),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].representative.session_id, "sessA");
    }

    /// Representative is the most-recent matching message per session.
    #[test]
    fn test_search_grouped_representative_is_best_scoring() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sessA",
            "/proj",
            "A",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        // The scores must be unambiguous: a tight match vs. a wall of filler that merely
        // contains the term. Two similar-length messages would let float noise decide.
        insert_test_message(
            &conn,
            "old_tight",
            "sessA",
            "match",
            "user",
            "2025-01-15T09:00:00",
            "/proj",
        );
        let padded = format!("{} match {}", "filler ".repeat(300), "filler ".repeat(300));
        insert_test_message(
            &conn,
            "new_padded",
            "sessA",
            &padded,
            "user",
            "2025-01-15T11:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let result = searcher
            .search_grouped_by_session(
                "match",
                &SearchFilter {
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].representative.message_uuid, "old_tight",
            "representative is the best-scoring message, not the most recent"
        );
        assert_eq!(result.rows[0].match_count, 2);

        // --sort=recent restores the most-recent representative.
        let result = searcher
            .search_grouped_by_session(
                "match",
                &SearchFilter {
                    limit: 5,
                    sort: SortOrder::Recent,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows[0].representative.message_uuid, "new_padded");
        assert_eq!(result.rows[0].match_count, 2);
    }

    /// Session score is the BEST (min) bm25 among its matches, not the mean -- otherwise a
    /// session with one excellent match drowns in its own mediocre ones.
    #[test]
    fn test_grouped_session_score_is_best_message() {
        let conn = setup_test_db();
        for (sid, last) in [
            ("sessA", "2025-01-15T12:00:00"),
            ("sessB", "2025-01-15T13:00:00"),
        ] {
            insert_test_conversation(
                &conn,
                sid,
                "/proj",
                "s",
                "2025-01-15T09:00:00",
                last,
                "claude_code",
            );
        }
        let padded = format!(
            "{} rustacean {}",
            "filler ".repeat(300),
            "filler ".repeat(300)
        );

        // sessA: one excellent match plus mediocre ones.
        insert_test_message(
            &conn,
            "a_best",
            "sessA",
            "rustacean",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "a_mid",
            "sessA",
            &padded,
            "user",
            "2025-01-15T12:00:00",
            "/proj",
        );
        // sessB: only mediocre matches, but more recent.
        insert_test_message(
            &conn,
            "b_mid",
            "sessB",
            &padded,
            "user",
            "2025-01-15T13:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let result = searcher
            .search_grouped_by_session(
                "rustacean",
                &SearchFilter {
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].representative.session_id, "sessA");
        assert_eq!(result.rows[0].representative.message_uuid, "a_best");
        assert_eq!(result.rows[1].representative.session_id, "sessB");
    }

    /// The grouped path must never early-stop: `match_count` counts ALL post-filter
    /// matches in the session, so every batch has to be scanned.
    ///
    /// The fixture must exceed BATCH_SIZE (500), otherwise a smuggled-in early break
    /// would never fire and the test would pass vacuously.
    #[test]
    fn test_grouped_match_count_unaffected_by_ranking() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sessA",
            "/proj",
            "A",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        for i in 0..600 {
            insert_test_message(
                &conn,
                &format!("msg{}", i),
                "sessA",
                "rustacean content here",
                "user",
                "2025-01-15T10:00:00",
                "/proj",
            );
        }

        let mut searcher = ConversationSearch::from_connection(conn);
        let result = searcher
            .search_grouped_by_session(
                "rustacean",
                &SearchFilter {
                    limit: 1,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].match_count, 600,
            "match_count must cover all matches even with limit=1 and >1 batch"
        );
    }

    /// The grouped path builds its FTS query independently of `search_conversations`, so
    /// the OR join needs its own guard. `--group-by-session` is the mode the skill uses most.
    #[test]
    fn test_grouped_multi_term_or_join() {
        let conn = setup_test_db();
        for (sid, proj) in [("sessA", "/proj"), ("sessB", "/proj")] {
            insert_test_conversation(
                &conn,
                sid,
                proj,
                "s",
                "2025-01-15T09:00:00",
                "2025-01-15T10:00:00",
                "claude_code",
            );
        }
        insert_test_message(
            &conn,
            "both",
            "sessA",
            "rustacean programming language",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "one",
            "sessB",
            "rustacean is a term for rust users",
            "user",
            "2025-01-15T10:01:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let result = searcher
            .search_grouped_by_session(
                "rustac programm",
                &SearchFilter {
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(result.rows.len(), 2, "OR join: both sessions match");
        assert_eq!(
            result.rows[0].representative.session_id, "sessA",
            "the session matching both terms ranks first"
        );
    }

    /// Quoting is the ONLY way to get AND/phrase semantics now that bare multi-word
    /// queries are OR-joined, and it is what `--exact` relies on (cli.rs wraps the query
    /// in quotes). It also bypasses the term-length routing entirely. Guards both FTS paths.
    #[test]
    fn test_quoted_phrase_bypasses_or_join_and_short_term_fallback() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "contiguous",
            "sess1",
            "the rustacean programming guide",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "split",
            "sess1",
            "rustacean users write code; programming is fun",
            "user",
            "2025-01-15T10:01:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);

        // Quoted: contiguous phrase only, not an OR over the two words.
        let rows = searcher
            .search_conversations("\"rustacean programming\"", &default_filter())
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_uuid, "contiguous");

        // Same on the grouped path.
        let result = searcher
            .search_grouped_by_session(
                "\"rustacean programming\"",
                &SearchFilter {
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].representative.message_uuid, "contiguous");

        // A quoted query with a <3-char token still reaches FTS verbatim.
        let rows = searcher
            .search_conversations("\"is fun\"", &default_filter())
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_uuid, "split");
    }

    /// Meta conversation messages must be excluded on both the window-function path
    /// (empty/short query) and the FTS path. Regression guard for the
    /// `is_meta_conversation = FALSE` predicate in both SQL branches.
    #[test]
    fn test_search_grouped_excludes_meta_conversation() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sessA",
            "/proj",
            "A",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sessM",
            "/proj",
            "only-meta",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );

        // sessA: 1 normal + 1 meta (both contain "keyword").
        insert_test_message_full(
            &conn,
            "a1",
            "sessA",
            "keyword alpha",
            "user",
            "2025-01-15T11:00:00",
            "/proj",
            false,
            false,
        );
        insert_test_message_full(
            &conn,
            "a_meta",
            "sessA",
            "keyword alpha",
            "user",
            "2025-01-15T10:30:00",
            "/proj",
            true,
            false,
        );

        // sessM: meta-only — must not appear in results at all.
        insert_test_message_full(
            &conn,
            "m1",
            "sessM",
            "keyword alpha",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
            true,
            false,
        );

        let mut searcher = ConversationSearch::from_connection(conn);

        // FTS path.
        let result = searcher
            .search_grouped_by_session(
                "keyword",
                &SearchFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1, "sessM (meta-only) must be excluded");
        assert_eq!(result.rows[0].representative.session_id, "sessA");
        assert_eq!(
            result.rows[0].match_count, 1,
            "meta message must not inflate match_count"
        );

        // Window-function path (empty query).
        let result = searcher
            .search_grouped_by_session(
                "",
                &SearchFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].representative.session_id, "sessA");
        assert_eq!(result.rows[0].match_count, 1);
    }

    /// `--days` and `--since/--until/--date` are mutually exclusive on the grouped
    /// path too. Pins the validation so a future refactor can't silently drop it.
    #[test]
    fn test_search_grouped_days_with_since_errors() {
        let conn = setup_test_db();
        let mut searcher = ConversationSearch::from_connection(conn);
        let filter = SearchFilter {
            days_back: Some(7),
            since: Some("2025-01-01"),
            limit: 10,
            ..Default::default()
        };
        let err = searcher
            .search_grouped_by_session("anything", &filter)
            .unwrap_err();
        assert!(err.to_string().contains("Cannot use --days"));
    }

    /// Negative limit is rejected with a clear error — guards against silent
    /// "full scan" behavior from `limit as usize` wrap or SQLite's "negative = unlimited"
    /// interpretation.
    #[test]
    fn test_search_grouped_negative_limit_errors() {
        let conn = setup_test_db();
        let mut searcher = ConversationSearch::from_connection(conn);
        let err = searcher
            .search_grouped_by_session(
                "anything",
                &SearchFilter {
                    limit: -1,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("limit must be >= 0"));
    }

    // ---- get_conversation_context tests ----

    #[test]
    fn test_get_conversation_context() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );

        // Insert parent message
        insert_test_message(
            &conn,
            "parent1",
            "sess1",
            "parent message",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

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

        let result = searcher
            .get_conversation_context("nonexistent-uuid", 5)
            .unwrap();

        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("not found"));
    }

    // ---- get_conversation_tree tests ----

    #[test]
    fn test_get_conversation_tree() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "tree test",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );

        // Insert root message
        insert_test_message(
            &conn,
            "root1",
            "sess1",
            "root message",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

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

    /// Inserts a message with an explicit `summary` column value, which the
    /// production indexers never write. Only the summary-fallback tests need it.
    fn insert_test_message_with_summary(
        conn: &Connection,
        uuid: &str,
        session_id: &str,
        content: &str,
        summary: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, parent_uuid, is_sidechain, depth, timestamp, message_type, project_path, conversation_file, full_content, summary, is_meta_conversation, is_tool_noise) VALUES (?, ?, NULL, FALSE, 0, '2025-01-15T10:00:00', 'user', '/proj', 'test.jsonl', ?, ?, FALSE, FALSE)",
            rusqlite::params![uuid, session_id, content, summary],
        )
        .unwrap();
    }

    /// Registers a conversation plus one message so `tree` has something to return.
    fn insert_tree_fixture(conn: &Connection, session_id: &str, content: &str) {
        insert_test_conversation(
            conn,
            session_id,
            "/proj",
            "c",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_message_with_summary(
            conn,
            &format!("msg-{}", session_id),
            session_id,
            content,
            None,
        );
    }

    #[test]
    fn test_tree_resolves_unique_prefix() {
        let conn = setup_test_db();
        insert_tree_fixture(&conn, "1c538017-97e6-49d5-a5f2-5b062d6822de", "hello world");

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("1c538017").unwrap();

        assert!(result.error.is_none(), "error: {:?}", result.error);
        assert_eq!(result.total_messages, 1);
        assert_eq!(result.tree[0].summary.as_deref(), Some("hello world"));
    }

    #[test]
    fn test_tree_ambiguous_prefix_returns_inband_error() {
        let conn = setup_test_db();
        insert_tree_fixture(&conn, "abcd0001-aaaa", "first");
        insert_tree_fixture(&conn, "abcd0002-bbbb", "second");

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("abcd").unwrap();

        let error = result.error.expect("ambiguous prefix must report an error");
        assert!(error.contains("Ambiguous"), "unexpected message: {}", error);
        // Full phrase, not a bare "2": the fixture's session ids contain digits and would
        // satisfy a substring check whatever count the code actually reported.
        assert!(
            error.contains("matches 2 conversations"),
            "message must state the count: {}",
            error
        );
        assert!(result.tree.is_empty());
    }

    /// The prefix search must anchor at the start. A `%…%` pattern would match the
    /// fragment mid-UUID and hand back an unrelated conversation as if it were the one
    /// asked for -- worse than reporting nothing.
    #[test]
    fn test_tree_prefix_does_not_match_mid_uuid() {
        let conn = setup_test_db();
        insert_tree_fixture(&conn, "aaaa1111-1c538017-bbbb", "unrelated session");

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("1c538017").unwrap();

        let error = result.error.expect("mid-UUID fragment must not resolve");
        assert!(error.contains("not found"), "unexpected message: {}", error);
        assert!(result.tree.is_empty());
    }

    #[test]
    fn test_tree_exact_match_wins_over_prefix() {
        let conn = setup_test_db();
        insert_tree_fixture(&conn, "abcd", "exact session");
        insert_tree_fixture(&conn, "abcd-longer", "prefixed session");

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("abcd").unwrap();

        assert!(result.error.is_none(), "error: {:?}", result.error);
        assert_eq!(result.tree[0].summary.as_deref(), Some("exact session"));
    }

    #[test]
    fn test_tree_resolves_prefixed_source_id() {
        let conn = setup_test_db();
        insert_tree_fixture(&conn, "oc:9f8e7d6c-1234", "opencode session");

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("9f8e7d6c").unwrap();

        assert!(result.error.is_none(), "error: {:?}", result.error);
        assert_eq!(result.tree[0].summary.as_deref(), Some("opencode session"));
    }

    #[test]
    fn test_tree_summary_falls_back_to_content() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "c",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_message_with_summary(
            &conn,
            "m1",
            "sess1",
            "first line of the message\nsecond line",
            None,
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess1").unwrap();

        assert_eq!(
            result.tree[0].summary.as_deref(),
            Some("first line of the message")
        );
    }

    #[test]
    fn test_tree_summary_prefers_stored_value() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "c",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_message_with_summary(
            &conn,
            "m1",
            "sess1",
            "raw content line",
            Some("stored summary"),
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess1").unwrap();

        assert_eq!(result.tree[0].summary.as_deref(), Some("stored summary"));
    }

    #[test]
    fn test_tree_summary_ignores_blank_stored_value() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "c",
            "2025-01-15T09:00:00",
            "2025-01-15T11:00:00",
            "claude_code",
        );
        insert_test_message_with_summary(&conn, "m1", "sess1", "raw content line", Some("   "));

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess1").unwrap();

        assert_eq!(result.tree[0].summary.as_deref(), Some("raw content line"));
    }

    #[test]
    fn test_tree_falls_back_to_raw_transcript_for_orphan_conversation() {
        let conn = setup_test_db();
        let file = write_test_jsonl(&[
            r#"{"uuid":"root1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess-orphan","message":{"role":"user","content":"raw root message"}}"#,
            r#"{"uuid":"child1","parentUuid":"root1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sess-orphan","message":{"role":"assistant","content":"raw child message"}}"#,
        ]);
        insert_orphan_conversation(
            &conn,
            "sess-orphan",
            Some(file.path().to_string_lossy().as_ref()),
            2,
            "claude_code",
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-orphan").unwrap();

        assert!(result.error.is_none());
        let warning = result.warning.as_deref().unwrap_or("");
        assert!(warning.contains("raw transcript"), "warning: {}", warning);
        assert!(warning.contains("index --all"), "warning: {}", warning);
        assert!(
            !warning.contains("Showing"),
            "complete tree should have no discrepancy note: {}",
            warning
        );
        assert_eq!(result.total_messages, 2);
        assert_eq!(result.tree.len(), 1);
        assert_eq!(result.tree[0].message_uuid, "root1");
        assert_eq!(result.tree[0].children[0].message_uuid, "child1");
    }

    #[test]
    fn test_tree_orphan_without_transcript_returns_actionable_error() {
        let conn = setup_test_db();
        insert_orphan_conversation(
            &conn,
            "sess-missing",
            Some("/tmp/does-not-exist-ai-conversation-search.jsonl"),
            2,
            "claude_code",
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-missing").unwrap();

        assert!(result.tree.is_empty());
        let error = result.error.as_deref().unwrap_or("");
        assert!(error.contains("could not be read"), "error: {}", error);
        assert!(error.contains("index --all"), "error: {}", error);
    }

    #[test]
    fn test_tree_orphan_without_recorded_path_returns_error() {
        let conn = setup_test_db();
        insert_orphan_conversation(&conn, "sess-nopath", None, 2, "claude_code");

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-nopath").unwrap();

        assert!(result.tree.is_empty());
        let error = result.error.as_deref().unwrap_or("");
        assert!(
            error.contains("no raw transcript path is recorded"),
            "error: {}",
            error
        );
        assert!(error.contains("index --all"), "error: {}", error);
    }

    #[test]
    fn test_tree_raw_fallback_filters_other_session_messages() {
        let conn = setup_test_db();
        // Resume scenario: the file re-emits the parent under the old sessionId.
        let file = write_test_jsonl(&[
            r#"{"uuid":"root1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess-old","message":{"role":"user","content":"old session root"}}"#,
            r#"{"uuid":"child1","parentUuid":"root1","isSidechain":false,"timestamp":"2025-01-15T10:01:00Z","type":"assistant","sessionId":"sess-new","message":{"role":"assistant","content":"new session reply"}}"#,
        ]);
        insert_orphan_conversation(
            &conn,
            "sess-new",
            Some(file.path().to_string_lossy().as_ref()),
            2,
            "claude_code",
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-new").unwrap();

        assert!(result.error.is_none());
        // Other-session message is excluded; its child is promoted to root.
        assert_eq!(result.total_messages, 1);
        assert_eq!(result.tree.len(), 1);
        assert_eq!(result.tree[0].message_uuid, "child1");
        let warning = result.warning.as_deref().unwrap_or("");
        assert!(warning.contains("Showing 1 of 2"), "warning: {}", warning);
    }

    #[test]
    fn test_tree_raw_fallback_stamps_missing_session_id() {
        let conn = setup_test_db();
        let file = write_test_jsonl(&[
            r#"{"uuid":"root1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","message":{"role":"user","content":"no sessionId field"}}"#,
        ]);
        insert_orphan_conversation(
            &conn,
            "sess-stamp",
            Some(file.path().to_string_lossy().as_ref()),
            1,
            "claude_code",
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-stamp").unwrap();

        assert!(result.error.is_none());
        assert_eq!(result.tree.len(), 1);
        assert_eq!(result.tree[0].session_id, "sess-stamp");
    }

    #[test]
    fn test_tree_raw_fallback_corrupt_transcript_reports_parse_failure() {
        let conn = setup_test_db();
        let file = write_test_jsonl(&[r#"{"uuid":"broken"#, "not json at all"]);
        insert_orphan_conversation(
            &conn,
            "sess-corrupt",
            Some(file.path().to_string_lossy().as_ref()),
            2,
            "claude_code",
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-corrupt").unwrap();

        assert!(result.tree.is_empty());
        let error = result.error.as_deref().unwrap_or("");
        assert!(error.contains("could be parsed"), "error: {}", error);
        assert!(error.contains("corrupt"), "error: {}", error);
    }

    #[test]
    fn test_tree_raw_fallback_wrong_session_only_reports_other_sessions() {
        let conn = setup_test_db();
        let file = write_test_jsonl(&[
            r#"{"uuid":"root1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess-other","message":{"role":"user","content":"belongs elsewhere"}}"#,
        ]);
        insert_orphan_conversation(
            &conn,
            "sess-wanted",
            Some(file.path().to_string_lossy().as_ref()),
            1,
            "claude_code",
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-wanted").unwrap();

        assert!(result.tree.is_empty());
        let error = result.error.as_deref().unwrap_or("");
        assert!(
            error.contains("no messages for session sess-wanted"),
            "error: {}",
            error
        );
        assert!(
            error.contains("belong to other sessions"),
            "error: {}",
            error
        );
    }

    #[test]
    fn test_tree_orphan_non_claude_source_skips_raw_fallback() {
        let conn = setup_test_db();
        // Even with a Claude-Code-formatted file on disk, a codex row must not
        // be parsed with the Claude Code schema.
        let file = write_test_jsonl(&[
            r#"{"uuid":"root1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess-codex","message":{"role":"user","content":"hello"}}"#,
        ]);
        insert_orphan_conversation(
            &conn,
            "sess-codex",
            Some(file.path().to_string_lossy().as_ref()),
            1,
            "codex",
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-codex").unwrap();

        assert!(result.tree.is_empty());
        let error = result.error.as_deref().unwrap_or("");
        assert!(
            error.contains("only supported for Claude Code sessions"),
            "error: {}",
            error
        );
        assert!(
            !error.contains("index --all"),
            "no repair advice for non-claude rows: {}",
            error
        );
    }

    #[test]
    fn test_tree_sibling_session_zero_count_omits_repair_advice() {
        let conn = setup_test_db();
        let file = write_test_jsonl(&[
            r#"{"uuid":"root1","parentUuid":null,"isSidechain":false,"timestamp":"2025-01-15T10:00:00Z","type":"user","sessionId":"sess-sibling","message":{"role":"user","content":"attributed elsewhere"}}"#,
        ]);
        // message_count = 0: the indexer attributed this session's messages to
        // a sibling session; re-indexing reproduces the same state.
        insert_orphan_conversation(
            &conn,
            "sess-sibling",
            Some(file.path().to_string_lossy().as_ref()),
            0,
            "claude_code",
        );

        let searcher = ConversationSearch::from_connection(conn);
        let result = searcher.get_conversation_tree("sess-sibling").unwrap();

        assert!(result.error.is_none());
        let warning = result.warning.as_deref().unwrap_or("");
        assert!(warning.contains("sibling session"), "warning: {}", warning);
        assert!(
            !warning.contains("index --all"),
            "repair advice is wrong here: {}",
            warning
        );
    }

    // ---- list_recent_conversations tests ----

    #[test]
    fn test_list_recent_conversations() {
        let conn = setup_test_db();
        let recent_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj_a",
            "summary a",
            &recent_ts,
            &recent_ts,
            "claude_code",
        );
        insert_test_conversation(
            &conn,
            "sess2",
            "/proj_b",
            "summary b",
            &recent_ts,
            &recent_ts,
            "opencode",
        );

        let searcher = ConversationSearch::from_connection(conn);

        // List all recent
        let results = searcher
            .list_recent_conversations(&SearchFilter {
                days_back: Some(1),
                ..default_filter()
            })
            .unwrap();
        assert_eq!(results.rows.len(), 2);

        // Filter by project
        let results = searcher
            .list_recent_conversations(&SearchFilter {
                days_back: Some(1),
                project_path: Some("/proj_a"),
                ..default_filter()
            })
            .unwrap();
        assert_eq!(results.rows.len(), 1);
        assert_eq!(results.rows[0].project_path.as_deref(), Some("/proj_a"));

        // Filter by source
        let results = searcher
            .list_recent_conversations(&SearchFilter {
                days_back: Some(1),
                source: Some("opencode"),
                ..default_filter()
            })
            .unwrap();
        assert_eq!(results.rows.len(), 1);
        assert_eq!(results.rows[0].source.as_deref(), Some("opencode"));
    }

    #[test]
    fn test_list_recent_conversations_reports_truncation() {
        let conn = setup_test_db();
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        for session in ["sess1", "sess2", "sess3"] {
            insert_test_conversation(&conn, session, "/proj", "summary", &ts, &ts, "claude_code");
        }
        let searcher = ConversationSearch::from_connection(conn);

        let capped = searcher
            .list_recent_conversations(&SearchFilter {
                days_back: Some(1),
                limit: 2,
                ..default_filter()
            })
            .unwrap();
        assert_eq!(capped.rows.len(), 2);
        assert!(capped.truncated);

        // Exactly `limit` matches is not truncation. This is the case the over-fetched
        // row exists to distinguish, and the one a naive `rows.len() == limit` check
        // would get wrong.
        let exact = searcher
            .list_recent_conversations(&SearchFilter {
                days_back: Some(1),
                limit: 3,
                ..default_filter()
            })
            .unwrap();
        assert_eq!(exact.rows.len(), 3);
        assert!(!exact.truncated);
    }

    #[test]
    fn test_list_recent_conversations_limit_zero_still_reports_truncation() {
        // `--limit 0` returns nothing while conversations exist; without the flag this
        // reads as "no conversations found".
        let conn = setup_test_db();
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        insert_test_conversation(&conn, "sess1", "/proj", "summary", &ts, &ts, "claude_code");
        let searcher = ConversationSearch::from_connection(conn);

        let result = searcher
            .list_recent_conversations(&SearchFilter {
                days_back: Some(1),
                limit: 0,
                ..default_filter()
            })
            .unwrap();
        assert!(result.rows.is_empty());
        assert!(result.truncated);
    }

    #[test]
    fn test_list_recent_conversations_rejects_negative_limit() {
        let conn = setup_test_db();
        let searcher = ConversationSearch::from_connection(conn);

        let err = searcher
            .list_recent_conversations(&SearchFilter {
                days_back: Some(1),
                limit: -1,
                ..default_filter()
            })
            .expect_err("negative limit must be rejected");
        assert!(err.to_string().contains("limit must be >= 0"));
    }

    // ---- query sanitization tests ----

    #[test]
    fn test_query_sanitization() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "rustacean programming language",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);

        // Single term — trigram substring match
        let results = searcher
            .search_conversations("rustac", &default_filter())
            .unwrap()
            .rows;
        assert_eq!(results.len(), 1); // "rustac" is a substring of "rustacean"

        // Multi terms — OR join, each as substring match
        let results = searcher
            .search_conversations("rustac programm", &default_filter())
            .unwrap()
            .rows;
        // Single message in this fixture, so OR and AND agree here; cardinality is pinned
        // by test_search_multi_term_or_join_ranked instead.
        assert_eq!(results.len(), 1);
    }

    // ---- Japanese / CJK search tests ----

    #[test]
    fn test_search_japanese_single_term() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "認証機能の実装を行いました",
            "assistant",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess1",
            "hello world in english",
            "user",
            "2025-01-15T10:01:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("認証", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_japanese_multi_term() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "認証機能の実装を行いました",
            "assistant",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess1",
            "認証だけのメッセージ",
            "user",
            "2025-01-15T10:01:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        // Both terms must match (AND join)
        let results = searcher
            .search_conversations("認証 実装", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1"); // Only msg1 contains both 認証 and 実装
    }

    #[test]
    fn test_search_mixed_cjk_english() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "OAuth認証の実装をRustで行いました",
            "assistant",
            "2025-01-15T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);

        // English term in mixed content
        let results = searcher
            .search_conversations("OAuth", &default_filter())
            .unwrap()
            .rows;
        assert_eq!(results.len(), 1);

        // Japanese term in mixed content
        let results = searcher
            .search_conversations("認証", &default_filter())
            .unwrap()
            .rows;
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_short_query_like_fallback() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "型の定義を変更しました",
            "assistant",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess1",
            "英語のメッセージ",
            "user",
            "2025-01-15T10:01:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        // 1-byte short query (e.g. "ab") should use LIKE fallback
        let results = searcher
            .search_conversations("ab", &default_filter())
            .unwrap()
            .rows;
        assert!(results.is_empty()); // no match, but should not error

        // CJK 2-char query "型の" (6 bytes) should use LIKE fallback
        let results = searcher
            .search_conversations("型の", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_uuid, "msg1");
    }

    #[test]
    fn test_search_cjk_2char_needs_like_fallback() {
        // SQLite trigram operates on codepoints, not bytes.
        // CJK 2-char terms (e.g. "認証") = 2 codepoints < 3, so FTS won't match.
        // LIKE fallback is required.
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "認証機能の実装を行いました",
            "assistant",
            "2025-01-15T10:00:00",
            "/proj",
        );

        // Direct FTS MATCH with CJK 2-char term should NOT match
        let result: Option<String> = conn
            .query_row(
                "SELECT message_uuid FROM message_content_fts WHERE full_content MATCH '\"認証\"'",
                [],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(
            result, None,
            "CJK 2-char term should NOT match via FTS trigram (2 codepoints < 3)"
        );

        // But search_conversations should still find it via LIKE fallback
        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("認証", &default_filter())
            .unwrap()
            .rows;
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_multi_term_or_join_ranked() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "rustacean programming language",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_test_message(
            &conn,
            "msg2",
            "sess1",
            "rustacean is a term for rust users",
            "user",
            "2025-01-15T10:01:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        // Multi-term queries are OR-joined, so both messages match; bm25 puts the one
        // containing BOTH terms first. AND-joining returned nothing useful in practice.
        let results = searcher
            .search_conversations("rustac programm", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].message_uuid, "msg1",
            "the document matching both terms must rank first"
        );
    }

    #[test]
    fn test_search_snippet_trigram() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "sess1",
            "/proj",
            "summary",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "msg1",
            "sess1",
            "the quick brown fox jumps over the lazy dog",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );

        let mut searcher = ConversationSearch::from_connection(conn);
        let results = searcher
            .search_conversations("brown fox", &default_filter())
            .unwrap()
            .rows;

        assert_eq!(results.len(), 1);
        let snippet = &results[0].context_snippet;
        // Snippet should contain highlighted search terms
        assert!(
            snippet.contains("**brown**"),
            "snippet should highlight 'brown': {}",
            snippet
        );
        assert!(
            snippet.contains("**fox**"),
            "snippet should highlight 'fox': {}",
            snippet
        );
    }

    #[test]
    fn test_extract_snippet_japanese_no_char_boundary_panic() {
        let content = "claude codeのsessionをcodexで引き継ぎたいときはどうしたらいい？";
        let snippet = extract_snippet(content, &["どう"], 20);
        assert!(
            snippet.contains("どう"),
            "snippet should contain Japanese match: {}",
            snippet
        );
    }

    #[test]
    fn test_extract_snippet_unicode_case_insensitive() {
        let snippet = extract_snippet("Try Café search", &["café"], 40);
        assert!(
            snippet.contains("**Café**"),
            "snippet should highlight Unicode case match: {}",
            snippet
        );
    }

    #[test]
    fn test_status_counts_orphan_conversations() {
        let conn = setup_test_db();
        insert_test_conversation(
            &conn,
            "healthy",
            "/proj",
            "healthy",
            "2025-01-15T09:00:00",
            "2025-01-15T10:00:00",
            "claude_code",
        );
        insert_test_message(
            &conn,
            "healthy-msg",
            "healthy",
            "hello",
            "user",
            "2025-01-15T10:00:00",
            "/proj",
        );
        insert_orphan_conversation(&conn, "orphan", Some("missing.jsonl"), 5, "claude_code");
        // Not orphans: legitimately-empty row (sibling-session attribution) and
        // a non-claude row outside the repair path's scope.
        insert_orphan_conversation(
            &conn,
            "empty-by-design",
            Some("missing.jsonl"),
            0,
            "claude_code",
        );
        insert_orphan_conversation(&conn, "codex-orphan", Some("rollout.jsonl"), 3, "codex");

        let searcher = ConversationSearch::from_connection(conn);
        let status = searcher.get_index_status(2).unwrap();

        assert_eq!(status.orphan_conversations, 1);
    }
}
