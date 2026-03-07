#!/usr/bin/env python3
"""Git utilities for resolving repository roots from project paths."""

import subprocess
from pathlib import Path
from typing import Optional


def resolve_repo_root(filesystem_path: str) -> Optional[str]:
    """
    Resolve the git repository root for a given filesystem path.

    For worktrees, resolves to the main repository root (not the worktree path).
    Returns None for non-git directories or invalid paths.
    """
    path = Path(filesystem_path).expanduser()
    if not path.exists():
        return None

    try:
        # First get the git common dir to handle worktrees
        result = subprocess.run(
            ["git", "-C", str(path), "rev-parse", "--git-common-dir"],
            capture_output=True, text=True, timeout=5
        )
        if result.returncode != 0:
            return None

        git_common_dir = result.stdout.strip()
        common_path = Path(git_common_dir)

        # If it's an absolute path, the repo root is its parent
        if common_path.is_absolute():
            # .git dir -> parent is repo root
            if common_path.name == ".git":
                return str(common_path.parent)
            return str(common_path.parent)

        # Relative path: resolve relative to the input path (CWD for git)
        resolved_common = (path / git_common_dir).resolve()

        # .git dir -> parent is repo root
        if resolved_common.name == ".git":
            return str(resolved_common.parent)
        return str(resolved_common.parent)

    except (subprocess.TimeoutExpired, OSError):
        return None
