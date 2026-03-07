#!/usr/bin/env python3
"""
OpenCode Conversation Indexer
Reads conversations from OpenCode's SQLite database and indexes them
into the same search database used by Claude Code conversations.
"""

import json
import os
import sqlite3
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional

from conversation_search.core.db import connect as connect_db
from conversation_search.core.git_utils import resolve_repo_root


# Default OpenCode database path
DEFAULT_OPENCODE_DB = "~/.local/share/opencode/opencode.db"


def get_opencode_db_path() -> str:
    """Resolve OpenCode DB path, respecting OPENCODE_HOME env var."""
    opencode_home = os.environ.get("OPENCODE_HOME")
    if opencode_home:
        return os.path.join(opencode_home, "opencode.db")
    return DEFAULT_OPENCODE_DB

# Session ID prefix to avoid collisions with Claude Code sessions
OC_PREFIX = "oc:"


class OpenCodeIndexer:
    def __init__(self, search_db_path: str = "~/.conversation-search/index.db",
                 opencode_db_path: Optional[str] = None, quiet: bool = False):
        self.search_db_path = Path(search_db_path).expanduser()
        self.opencode_db_path = Path(opencode_db_path or get_opencode_db_path()).expanduser()
        self.quiet = quiet
        self._repo_root_cache = {}

    def _log(self, msg: str):
        if not self.quiet:
            print(msg)

    def _connect_opencode(self) -> Optional[sqlite3.Connection]:
        """Connect to OpenCode DB in read-only mode."""
        if not self.opencode_db_path.exists():
            self._log(f"OpenCode DB not found: {self.opencode_db_path}")
            return None

        conn = sqlite3.connect(f"file:{self.opencode_db_path}?mode=ro", uri=True, timeout=10.0)
        conn.row_factory = sqlite3.Row
        return conn

    def _connect_search_db(self) -> sqlite3.Connection:
        """Connect to the search database."""
        return connect_db(str(self.search_db_path))

    def _ensure_sync_table(self, conn: sqlite3.Connection):
        """Create the sync state table if needed."""
        conn.execute("""
            CREATE TABLE IF NOT EXISTS opencode_sync_state (
                key TEXT PRIMARY KEY,
                value TEXT
            )
        """)
        conn.commit()

    def _get_last_sync_time(self, conn: sqlite3.Connection) -> Optional[int]:
        """Get the last sync timestamp (epoch ms)."""
        try:
            cursor = conn.execute(
                "SELECT value FROM opencode_sync_state WHERE key = 'last_sync_time'"
            )
            row = cursor.fetchone()
            return int(row['value']) if row else None
        except sqlite3.OperationalError:
            return None

    def _set_last_sync_time(self, conn: sqlite3.Connection, time_ms: int):
        """Set the last sync timestamp (epoch ms)."""
        conn.execute(
            "INSERT OR REPLACE INTO opencode_sync_state (key, value) VALUES ('last_sync_time', ?)",
            (str(time_ms),)
        )
        conn.commit()

    def _resolve_repo_root_cached(self, worktree: str) -> Optional[str]:
        """Resolve repo root with in-memory cache.

        Caches both successful results and None for non-existent paths
        to avoid repeated git commands for the same worktree.
        """
        if worktree in self._repo_root_cache:
            return self._repo_root_cache[worktree]

        repo_root = resolve_repo_root(worktree)
        self._repo_root_cache[worktree] = repo_root
        return repo_root

    def _epoch_ms_to_iso(self, epoch_ms: int) -> str:
        """Convert epoch milliseconds to ISO 8601 UTC string."""
        dt = datetime.fromtimestamp(epoch_ms / 1000, tz=timezone.utc)
        return dt.strftime('%Y-%m-%dT%H:%M:%S.') + f"{dt.microsecond // 1000:03d}Z"

    def _build_message_content(self, parts: list) -> str:
        """Build text content from message parts."""
        text_parts = []
        for part_row in parts:
            try:
                data = json.loads(part_row['data'])
            except (json.JSONDecodeError, TypeError):
                continue

            part_type = data.get('type', '')

            if part_type == 'text':
                text = data.get('text', '')
                if text:
                    text_parts.append(text)
            elif part_type == 'tool':
                tool_name = data.get('tool', 'unknown')
                text_parts.append(f"[Tool: {tool_name}]")
                # Include tool input for searchability
                state = data.get('state', {})
                if isinstance(state, dict):
                    tool_input = state.get('input', {})
                    if isinstance(tool_input, dict):
                        command = tool_input.get('command', '')
                        if command:
                            text_parts.append(command)
            elif part_type in ('patch', 'file'):
                text_parts.append("[File change]")
            # Skip: reasoning, step-start, step-finish, compaction

        return '\n'.join(text_parts)

    def scan_and_index(self, days_back: Optional[int] = 1) -> int:
        """
        Scan OpenCode sessions and index them into the search database.

        Args:
            days_back: Only index sessions updated in the last N days.
                      None = use incremental sync (only new since last sync).

        Returns:
            Number of sessions indexed.
        """
        oc_conn = self._connect_opencode()
        if not oc_conn:
            return 0

        search_conn = self._connect_search_db()
        self._ensure_sync_table(search_conn)

        try:
            return self._do_index(oc_conn, search_conn, days_back)
        finally:
            oc_conn.close()
            search_conn.close()

    def _do_index(self, oc_conn: sqlite3.Connection, search_conn: sqlite3.Connection,
                  days_back: Optional[int]) -> int:
        """Core indexing logic."""
        # Determine cutoff time
        last_sync = self._get_last_sync_time(search_conn)

        if days_back is not None:
            cutoff_ms = int((datetime.now() - timedelta(days=days_back)).timestamp() * 1000)
        elif last_sync:
            cutoff_ms = last_sync
        else:
            # First run with no days_back: index everything
            cutoff_ms = 0

        # Fetch sessions updated since cutoff
        sessions = oc_conn.execute("""
            SELECT s.id, s.project_id, s.title, s.directory,
                   s.time_created, s.time_updated,
                   p.worktree
            FROM session s
            JOIN project p ON s.project_id = p.id
            WHERE s.time_updated > ?
            ORDER BY s.time_updated DESC
        """, (cutoff_ms,)).fetchall()

        if not sessions:
            self._log("No new OpenCode sessions to index")
            return 0

        self._log(f"Found {len(sessions)} OpenCode sessions to index")

        max_time_updated = cutoff_ms
        indexed_count = 0

        for session in sessions:
            try:
                count = self._index_session(oc_conn, search_conn, session)
                if count > 0:
                    indexed_count += 1
                if session['time_updated'] > max_time_updated:
                    max_time_updated = session['time_updated']
            except Exception as e:
                self._log(f"  Error indexing session {session['id']}: {e}")
                continue

        # Commit all sessions at once
        search_conn.commit()

        # Update sync time
        if max_time_updated > cutoff_ms:
            self._set_last_sync_time(search_conn, max_time_updated)

        self._log(f"Indexed {indexed_count} OpenCode sessions")
        return indexed_count

    def _index_session(self, oc_conn: sqlite3.Connection, search_conn: sqlite3.Connection,
                       session) -> int:
        """Index a single OpenCode session. Returns number of messages indexed."""
        session_id = OC_PREFIX + session['id']
        worktree = session['worktree'] or session['directory']

        # Check if already indexed with same update time
        cursor = search_conn.cursor()
        cursor.execute(
            "SELECT last_message_at FROM conversations WHERE session_id = ?",
            (session_id,)
        )
        existing = cursor.fetchone()

        session_updated_iso = self._epoch_ms_to_iso(session['time_updated'])

        if existing and existing['last_message_at'] == session_updated_iso:
            return 0  # Already up to date

        # Fetch messages for this session
        messages = oc_conn.execute("""
            SELECT id, session_id, time_created, time_updated, data
            FROM message
            WHERE session_id = ?
            ORDER BY time_created ASC
        """, (session['id'],)).fetchall()

        if not messages:
            return 0

        # Fetch all parts for this session
        parts_by_message = {}
        parts = oc_conn.execute("""
            SELECT id, message_id, data, time_created
            FROM part
            WHERE session_id = ?
            ORDER BY time_created ASC
        """, (session['id'],)).fetchall()

        for part in parts:
            msg_id = part['message_id']
            if msg_id not in parts_by_message:
                parts_by_message[msg_id] = []
            parts_by_message[msg_id].append(part)

        # Resolve repo root
        repo_root = self._resolve_repo_root_cached(worktree) if worktree else None

        # Delete existing data for this session (full re-index per session)
        if existing:
            cursor.execute("DELETE FROM messages WHERE session_id = ?", (session_id,))

        # Build and insert messages
        msg_count = 0
        first_timestamp = None
        last_timestamp = None

        for msg in messages:
            try:
                msg_data = json.loads(msg['data'])
            except (json.JSONDecodeError, TypeError):
                continue

            role = msg_data.get('role', 'unknown')
            if role not in ('user', 'assistant'):
                continue

            msg_parts = parts_by_message.get(msg['id'], [])
            content = self._build_message_content(msg_parts)

            if not content.strip():
                continue

            timestamp = self._epoch_ms_to_iso(msg['time_created'])
            message_uuid = OC_PREFIX + msg['id']

            if first_timestamp is None:
                first_timestamp = timestamp
            last_timestamp = timestamp

            cursor.execute("""
                INSERT OR REPLACE INTO messages (
                    message_uuid, session_id, parent_uuid, is_sidechain,
                    depth, timestamp, message_type, project_path,
                    conversation_file, full_content, is_meta_conversation,
                    is_tool_noise
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """, (
                message_uuid,
                session_id,
                None,  # OpenCode doesn't have tree structure
                False,
                msg_count,  # Use sequential depth
                timestamp,
                role,
                worktree,
                str(self.opencode_db_path),
                content,
                False,
                False,
            ))
            msg_count += 1

        if msg_count == 0:
            return 0

        # Upsert conversation metadata
        title = session['title'] or 'Untitled'
        session_created_iso = self._epoch_ms_to_iso(session['time_created'])

        cursor.execute("""
            INSERT OR REPLACE INTO conversations (
                session_id, project_path, repo_root, conversation_file,
                root_message_uuid, leaf_message_uuid, conversation_summary,
                first_message_at, last_message_at, message_count, source
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'opencode')
        """, (
            session_id,
            worktree,
            repo_root,
            str(self.opencode_db_path),
            OC_PREFIX + messages[0]['id'],
            OC_PREFIX + messages[-1]['id'],
            title,
            first_timestamp or session_created_iso,
            session_updated_iso,
            msg_count,
        ))

        self._log(f"  Indexed session: {title} ({msg_count} messages)")
        return msg_count
