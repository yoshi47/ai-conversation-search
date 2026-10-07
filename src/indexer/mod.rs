pub mod claude_code;
pub mod codex;
pub mod opencode;

use rusqlite::Connection;

pub use claude_code::count_conversation_files_on_disk;
pub use claude_code::warn_on_unrecognised_observer_flag;
pub use claude_code::ConversationIndexer;

/// Resolve repo root with DB-backed cache.
pub fn resolve_repo_root_cached(conn: &Connection, project_path: &str) -> Option<String> {
    // Check cache
    if let Ok(cached) = conn.query_row(
        "SELECT repo_root FROM repo_root_cache WHERE project_path = ?",
        [project_path],
        |row| row.get::<_, Option<String>>(0),
    ) {
        return cached;
    }

    let result = crate::git_utils::resolve_repo_root(project_path);

    // Cache result (including None to avoid repeated git lookups)
    let _ = conn.execute(
        "INSERT OR REPLACE INTO repo_root_cache (project_path, repo_root) VALUES (?, ?)",
        rusqlite::params![project_path, result.as_deref()],
    );

    result
}

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
    /// Model that produced this message (assistant only; None for user/unknown).
    pub model: Option<String>,
}

/// Metadata extracted from a conversation JSONL file.
#[derive(Debug)]
pub struct ConversationMeta {
    pub summary: Option<String>,
    pub leaf_uuid: Option<String>,
    pub custom_title: Option<String>,
    pub first_user_message: Option<String>,
}

/// Join distinct non-empty models in first-seen order, for `conversations.model`.
///
/// Returns None when no message carries a model, so "unknown" stays NULL
/// rather than an empty string that `list` would render as a blank row.
pub fn distinct_model_list(models: &[Option<String>]) -> Option<String> {
    let mut seen: Vec<&str> = Vec::new();
    for m in models.iter().flatten() {
        let m = m.trim();
        if !m.is_empty() && !seen.contains(&m) {
            seen.push(m);
        }
    }
    if seen.is_empty() {
        None
    } else {
        Some(seen.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distinct_model_list_dedupes_keeps_order_and_skips_blanks() {
        assert_eq!(distinct_model_list(&[]), None);
        assert_eq!(distinct_model_list(&[None, None]), None);
        assert_eq!(
            distinct_model_list(&[
                Some("b".to_string()),
                None,
                Some("a".to_string()),
                Some("b".to_string()),
                Some("  ".to_string()),
            ]),
            Some("b,a".to_string())
        );
    }
}
