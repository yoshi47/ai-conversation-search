use std::path::PathBuf;
use std::process::Command;

/// Resolve the git repository root for a given filesystem path.
///
/// For worktrees, resolves to the main repository root (not the worktree path).
/// Returns None for non-git directories or invalid paths.
pub fn resolve_repo_root(filesystem_path: &str) -> Option<String> {
    let path = crate::db::expand_path(filesystem_path);
    if !path.exists() {
        return None;
    }

    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-parse", "--git-common-dir"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let git_common_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let common_path = PathBuf::from(&git_common_dir);

    let resolved = if common_path.is_absolute() {
        common_path
    } else {
        // Relative path: resolve relative to the input path
        match (path.join(&git_common_dir)).canonicalize() {
            Ok(p) => p,
            Err(_) => return None,
        }
    };

    // .git dir -> parent is repo root
    if resolved.file_name().map_or(false, |n| n == ".git") {
        resolved.parent().map(|p| p.to_string_lossy().to_string())
    } else {
        Some(resolved.to_string_lossy().to_string())
    }
}
