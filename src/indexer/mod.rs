pub mod claude_code;
pub mod codex;
pub mod opencode;

use rusqlite::Connection;

pub use claude_code::ConversationIndexer;
pub use claude_code::count_conversation_files_on_disk;

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
}

/// Metadata extracted from a conversation JSONL file.
#[derive(Debug)]
pub struct ConversationMeta {
    pub summary: Option<String>,
    pub leaf_uuid: Option<String>,
    pub custom_title: Option<String>,
    pub first_user_message: Option<String>,
}
