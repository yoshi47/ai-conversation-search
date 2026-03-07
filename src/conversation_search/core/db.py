"""Shared database connection utilities."""

import sqlite3
from pathlib import Path


DEFAULT_DB_PATH = "~/.conversation-search/index.db"


def connect(db_path: str = DEFAULT_DB_PATH, readonly: bool = False) -> sqlite3.Connection:
    """Create a SQLite connection with standard PRAGMA settings.

    Args:
        db_path: Path to the database file (supports ~ expansion).
        readonly: If True, open in read-only mode.

    Returns:
        Configured sqlite3.Connection with Row factory.
    """
    resolved = str(Path(db_path).expanduser())

    if readonly:
        conn = sqlite3.connect(f"file:{resolved}?mode=ro", uri=True, timeout=10.0)
    else:
        conn = sqlite3.connect(resolved, timeout=30.0)

    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute("PRAGMA busy_timeout=30000")
    conn.row_factory = sqlite3.Row
    return conn
