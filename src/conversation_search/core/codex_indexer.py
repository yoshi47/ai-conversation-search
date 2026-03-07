#!/usr/bin/env python3
"""
Codex CLI Conversation Indexer
Reads conversations from Codex CLI's JSONL session files and indexes them
into the same search database used by Claude Code and OpenCode conversations.
"""

import json
import re
import sqlite3
from datetime import datetime, timedelta
from pathlib import Path
from typing import Optional

from conversation_search.core.db import connect as connect_db
from conversation_search.core.git_utils import resolve_repo_root


# Default Codex sessions directory
DEFAULT_CODEX_SESSIONS = "~/.codex/sessions"

# Session ID prefix to avoid collisions
CX_PREFIX = "codex:"


class CodexIndexer:
    def __init__(self, search_db_path: str = "~/.conversation-search/index.db",
                 sessions_dir: Optional[str] = None, quiet: bool = False):
        self.search_db_path = Path(search_db_path).expanduser()
        self.sessions_dir = Path(sessions_dir or DEFAULT_CODEX_SESSIONS).expanduser()
        self.quiet = quiet
        self._repo_root_cache = {}

    def _log(self, msg: str):
        if not self.quiet:
            print(msg)

    def _connect_search_db(self) -> sqlite3.Connection:
        """Connect to the search database."""
        return connect_db(str(self.search_db_path))

    def _ensure_sync_table(self, conn: sqlite3.Connection):
        """Create the sync state table if needed."""
        conn.execute("""
            CREATE TABLE IF NOT EXISTS codex_sync_state (
                file_path TEXT PRIMARY KEY,
                mtime REAL NOT NULL,
                indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
            )
        """)
        conn.commit()

    def _resolve_repo_root_cached(self, worktree: str) -> Optional[str]:
        """Resolve repo root with in-memory cache."""
        if worktree in self._repo_root_cache:
            return self._repo_root_cache[worktree]

        repo_root = resolve_repo_root(worktree)
        self._repo_root_cache[worktree] = repo_root
        return repo_root

    def _find_session_files(self, days_back: Optional[int] = None) -> list[Path]:
        """Find JSONL session files, optionally filtering by date directories."""
        if not self.sessions_dir.exists():
            return []

        if days_back is not None:
            # Use date-based directory structure to skip old files
            files = []
            today = datetime.now()
            for d in range(days_back + 1):
                date = today - timedelta(days=d)
                day_dir = self.sessions_dir / str(date.year) / f"{date.month:02d}" / f"{date.day:02d}"
                if day_dir.exists():
                    files.extend(day_dir.glob("*.jsonl"))
            return sorted(files)

        return sorted(self.sessions_dir.rglob("*.jsonl"))

    def scan_and_index(self, days_back: Optional[int] = 1) -> int:
        """
        Scan Codex session files and index them into the search database.

        Args:
            days_back: Only index sessions from the last N days.
                      None = scan all files.

        Returns:
            Number of sessions indexed.
        """
        session_files = self._find_session_files(days_back)
        if not session_files:
            self._log("No Codex session files found")
            return 0

        conn = self._connect_search_db()
        self._ensure_sync_table(conn)

        try:
            return self._do_index(conn, session_files)
        finally:
            conn.close()

    def _do_index(self, conn: sqlite3.Connection, session_files: list[Path]) -> int:
        """Core indexing logic."""
        indexed_count = 0

        for session_file in session_files:
            try:
                mtime = session_file.stat().st_mtime

                # Check if already indexed with same mtime
                cursor = conn.execute(
                    "SELECT mtime FROM codex_sync_state WHERE file_path = ?",
                    (str(session_file),)
                )
                existing = cursor.fetchone()
                if existing and existing['mtime'] == mtime:
                    continue

                count = self._index_session_file(conn, session_file)
                if count > 0:
                    indexed_count += 1

                # Update sync state
                conn.execute(
                    "INSERT OR REPLACE INTO codex_sync_state (file_path, mtime) VALUES (?, ?)",
                    (str(session_file), mtime)
                )
            except (OSError, sqlite3.Error, json.JSONDecodeError, ValueError) as e:
                self._log(f"  Error indexing {session_file.name}: {e}")
                continue

        conn.commit()

        if indexed_count > 0:
            self._log(f"Indexed {indexed_count} Codex sessions")
        else:
            self._log("No new Codex sessions to index")

        return indexed_count

    _UUID_RE = re.compile(r'([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})', re.IGNORECASE)

    def _extract_uuid_from_filename(self, filename: str) -> str:
        """Extract UUID from filename like rollout-2025-11-04T12-05-46-019a4cd3-e3a2-7f73-9181-4293b1a25f23.jsonl"""
        match = self._UUID_RE.search(filename)
        if match:
            return match.group(1)
        return Path(filename).stem

    def _index_session_file(self, conn: sqlite3.Connection, session_file: Path) -> int:
        """Index a single Codex session file. Returns number of messages indexed."""
        lines = session_file.read_text(encoding='utf-8', errors='replace').strip().split('\n')
        if not lines:
            return 0

        # Parse first line for session metadata
        try:
            first = json.loads(lines[0])
        except json.JSONDecodeError as e:
            self._log(f"  Skipping {session_file.name}: invalid JSON on line 1: {e}")
            return 0

        if first.get('type') != 'session_meta':
            return 0

        payload = first.get('payload', {})
        session_uuid = payload.get('id') or self._extract_uuid_from_filename(session_file.name)
        session_id = CX_PREFIX + session_uuid
        cwd = payload.get('cwd', '')

        # Resolve repo root
        repo_root = self._resolve_repo_root_cached(cwd) if cwd else None

        # Delete existing data for this session (full re-index)
        conn.execute("DELETE FROM messages WHERE session_id = ?", (session_id,))

        # Parse all events
        messages = []
        msg_count = 0
        first_timestamp = None
        last_timestamp = None
        title_parts = []

        for line in lines[1:]:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue

            event_type = event.get('type')
            event_payload = event.get('payload', {})
            timestamp = event.get('timestamp', '')

            if event_type == 'event_msg':
                msg_type = event_payload.get('type')

                if msg_type == 'user_message':
                    text = event_payload.get('message', '')
                    if text.strip():
                        messages.append(('user', timestamp, text))
                        if not title_parts:
                            title_parts.append(text[:100])

                elif msg_type == 'agent_message':
                    text = event_payload.get('message', '')
                    if text.strip():
                        messages.append(('assistant', timestamp, text))

                elif msg_type == 'agent_reasoning':
                    text = event_payload.get('text', '')
                    if text.strip():
                        messages.append(('assistant', timestamp, f"[Reasoning] {text}"))

                # Skip: token_count, turn_context, etc.

            elif event_type == 'response_item':
                item_type = event_payload.get('type')

                if item_type == 'function_call':
                    name = event_payload.get('name', 'unknown')
                    args_str = event_payload.get('arguments', '')
                    tool_text = f"[Tool: {name}]"
                    # Try to extract command for searchability
                    try:
                        args = json.loads(args_str)
                        if isinstance(args, dict):
                            cmd = args.get('command', '')
                            if isinstance(cmd, list):
                                cmd = ' '.join(cmd)
                            if cmd:
                                tool_text += f"\n{cmd}"
                    except (json.JSONDecodeError, TypeError):
                        pass
                    messages.append(('assistant', timestamp, tool_text))

                elif item_type == 'function_call_output':
                    output_str = event_payload.get('output', '')
                    # Truncate large outputs
                    try:
                        output_data = json.loads(output_str)
                        if isinstance(output_data, dict):
                            output_text = output_data.get('output', '')[:500]
                        else:
                            output_text = str(output_data)[:500]
                    except (json.JSONDecodeError, TypeError):
                        output_text = str(output_str)[:500]
                    if output_text.strip():
                        messages.append(('assistant', timestamp, f"[Tool Output]\n{output_text}"))

                elif item_type == 'message':
                    role = event_payload.get('role', '')
                    # Skip user/developer context messages (instructions, environment)
                    if role in ('user', 'developer'):
                        continue
                    content_parts = event_payload.get('content', [])
                    for part in content_parts:
                        if isinstance(part, dict) and part.get('type') == 'output_text':
                            text = part.get('text', '')
                            if text.strip():
                                messages.append(('assistant', timestamp, text))

                # Skip: reasoning (encrypted)

        if not messages:
            return 0

        # Consolidate consecutive same-role messages
        consolidated = []
        for role, ts, text in messages:
            if consolidated and consolidated[-1][0] == role:
                consolidated[-1] = (role, consolidated[-1][1], consolidated[-1][2] + '\n' + text)
            else:
                consolidated.append((role, ts, text))

        # Insert messages
        cursor = conn.cursor()
        for i, (role, ts, content) in enumerate(consolidated):
            if not content.strip():
                continue

            message_uuid = f"{CX_PREFIX}{session_uuid}:{i}"

            if first_timestamp is None:
                first_timestamp = ts
            last_timestamp = ts

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
                None,
                False,
                i,
                ts,
                role,
                cwd,
                str(session_file),
                content,
                False,
                False,
            ))
            msg_count += 1

        if msg_count == 0:
            return 0

        # Build title from first user message
        title = title_parts[0] if title_parts else 'Untitled'

        # Upsert conversation metadata
        session_timestamp = payload.get('timestamp', first_timestamp or '')

        cursor.execute("""
            INSERT OR REPLACE INTO conversations (
                session_id, project_path, repo_root, conversation_file,
                root_message_uuid, leaf_message_uuid, conversation_summary,
                first_message_at, last_message_at, message_count, source
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'codex')
        """, (
            session_id,
            cwd,
            repo_root,
            str(session_file),
            f"{CX_PREFIX}{session_uuid}:0",
            f"{CX_PREFIX}{session_uuid}:{msg_count - 1}",
            title,
            first_timestamp or session_timestamp,
            last_timestamp or session_timestamp,
            msg_count,
        ))

        self._log(f"  Indexed session: {title[:60]} ({msg_count} messages)")
        return msg_count
