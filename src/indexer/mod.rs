pub mod claude_code;
pub mod codex;
pub mod opencode;

use rusqlite::Connection;

pub use claude_code::count_conversation_files_on_disk;
pub use claude_code::warn_on_unrecognised_observer_flag;
pub use claude_code::ConversationIndexer;

/// Resolve repo root with DB-backed cache.
pub fn resolve_repo_root_cached(conn: &Connection, project_path: &str) -> Option<String> {
    resolve_repo_root_cached_at(conn, project_path, Some(project_path))
}

/// Resolve the repo root of `lookup_path`, caching it under `cache_key`.
///
/// The two differ for Claude Code, whose cache key is the project path but whose
/// lookup path is decoded from the transcript's directory name. With no lookup path
/// only the cache is consulted.
///
/// Only hits are cached. A NULL row (written by earlier versions) is treated as a
/// miss: caching "not a repo" would pin a directory that is later `git init`-ed.
pub(crate) fn resolve_repo_root_cached_at(
    conn: &Connection,
    cache_key: &str,
    lookup_path: Option<&str>,
) -> Option<String> {
    if let Ok(Some(cached)) = conn.query_row(
        "SELECT repo_root FROM repo_root_cache WHERE project_path = ?",
        [cache_key],
        |row| row.get::<_, Option<String>>(0),
    ) {
        return Some(cached);
    }

    let root = crate::git_utils::resolve_repo_root(lookup_path?)?;

    // A failed write only costs a git lookup next time, so it must not fail indexing.
    if let Err(e) = conn.execute(
        "INSERT OR REPLACE INTO repo_root_cache (project_path, repo_root) VALUES (?, ?)",
        rusqlite::params![cache_key, root],
    ) {
        log::warn!("failed to cache repo_root for {}: {}", cache_key, e);
    }

    Some(root)
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

    fn cache_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE repo_root_cache (
                project_path TEXT PRIMARY KEY,
                repo_root TEXT,
                resolved_at TEXT DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .unwrap();
        conn
    }

    fn git_init(dir: &std::path::Path) {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir)
            .status()
            .expect("git must be installed to run this test");
        assert!(status.success());
    }

    fn canonical(dir: &std::path::Path) -> String {
        dir.canonicalize().unwrap().to_string_lossy().to_string()
    }

    #[test]
    fn test_resolve_repo_root_cached_resolves_after_git_init() {
        let conn = cache_conn();
        let dir = tempfile::tempdir().unwrap();
        let path = canonical(dir.path());

        assert_eq!(resolve_repo_root_cached(&conn, &path), None);

        git_init(dir.path());
        assert_eq!(resolve_repo_root_cached(&conn, &path), Some(path.clone()));
    }

    #[test]
    fn test_resolve_repo_root_cached_ignores_cached_null() {
        // Rows written as NULL by earlier versions must not pin the path to "no repo".
        let conn = cache_conn();
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let path = canonical(dir.path());
        conn.execute(
            "INSERT INTO repo_root_cache (project_path, repo_root) VALUES (?, NULL)",
            [&path],
        )
        .unwrap();

        assert_eq!(resolve_repo_root_cached(&conn, &path), Some(path.clone()));
        let cached: Option<String> = conn
            .query_row(
                "SELECT repo_root FROM repo_root_cache WHERE project_path = ?",
                [&path],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cached, Some(path));
    }

    #[test]
    fn test_resolve_repo_root_cached_does_not_store_none() {
        let conn = cache_conn();
        let dir = tempfile::tempdir().unwrap();
        let path = canonical(dir.path());

        assert_eq!(resolve_repo_root_cached(&conn, &path), None);
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM repo_root_cache", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

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
