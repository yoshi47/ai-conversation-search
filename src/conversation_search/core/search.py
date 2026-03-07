#!/usr/bin/env python3
"""
Conversation Search Search Tools
Provides search and retrieval tools for Claude to query conversation history
"""

import sqlite3
import sys
from pathlib import Path
from typing import List, Dict, Optional
from datetime import datetime, timedelta

from conversation_search.core.db import connect as connect_db
from conversation_search.core.date_utils import build_date_filter


def _escape_like(value: str) -> str:
    """Escape special LIKE characters (%, _, \\) for safe use in LIKE clauses."""
    return value.replace('\\', '\\\\').replace('%', '\\%').replace('_', '\\_')


def format_timestamp(iso_timestamp: str, include_date: bool = True, include_seconds: bool = False) -> str:
    """
    Convert UTC ISO timestamp to local time for display.

    Args:
        iso_timestamp: ISO format timestamp with Z suffix (UTC)
        include_date: Include date in output (default: True)
        include_seconds: Include seconds in time (default: False)

    Returns:
        Formatted timestamp in local timezone
    """
    dt_utc = datetime.fromisoformat(iso_timestamp.replace('Z', '+00:00'))
    dt_local = dt_utc.astimezone()

    if include_date:
        if include_seconds:
            return dt_local.strftime('%Y-%m-%d %H:%M:%S')
        else:
            return dt_local.strftime('%Y-%m-%d %H:%M')
    else:
        if include_seconds:
            return dt_local.strftime('%H:%M:%S')
        else:
            return dt_local.strftime('%H:%M')


class ConversationSearch:
    def __init__(self, db_path: str = "~/.conversation-search/index.db"):
        self.db_path = Path(db_path).expanduser()
        if not self.db_path.exists():
            raise FileNotFoundError(
                f"Database not found at {self.db_path}. "
                "Run the indexer first: python src/indexer.py"
            )
        self.conn = connect_db(db_path)
        self._fts_rebuilt = False

    def search_conversations(
        self,
        query: str,
        days_back: Optional[int] = None,
        since: Optional[str] = None,
        until: Optional[str] = None,
        date: Optional[str] = None,
        limit: int = 20,
        project_path: Optional[str] = None,
        repo: Optional[str] = None,
        snippet_tokens: int = 128,
        source: Optional[str] = None
    ) -> List[Dict]:
        """
        Search conversations using full-text search on complete content

        Args:
            query: Search query
            days_back: Limit to last N days (None = all time)
            since: Start date (YYYY-MM-DD, 'yesterday', 'today')
            until: End date (YYYY-MM-DD, 'yesterday', 'today')
            date: Specific date (YYYY-MM-DD, 'yesterday', 'today')
            limit: Maximum number of results
            project_path: Filter by project path
            snippet_tokens: Number of tokens to show around each match (default: 128)

        Returns:
            List of matching messages with context snippets
        """
        # Validate mutually exclusive date filters
        if days_back and (since or until or date):
            raise ValueError("Cannot use --days with --since/--until/--date")
        cursor = self.conn.cursor()

        # Handle empty query (match all)
        if not query or not query.strip():
            # No FTS search, just filter by dates/project
            sql = """
                SELECT
                    m.message_uuid,
                    m.session_id,
                    m.parent_uuid,
                    m.timestamp,
                    m.message_type,
                    m.project_path,
                    m.depth,
                    m.is_sidechain,
                    SUBSTR(m.full_content, 1, 500) as context_snippet,
                    c.conversation_summary,
                    c.conversation_file,
                    c.source
                FROM messages m
                JOIN conversations c ON m.session_id = c.session_id
                WHERE m.is_meta_conversation = FALSE
            """
            params = []
        else:
            # Sanitize query for FTS5
            fts_query = query
            if not any(op in query for op in [' AND ', ' OR ', ' NOT ', '"']):
                terms = query.split()
                if len(terms) == 1:
                    fts_query = f'{terms[0]}*'
                else:
                    fts_query = ' '.join(f'{term}*' for term in terms)

            sql = """
                SELECT
                    m.message_uuid,
                    m.session_id,
                    m.parent_uuid,
                    m.timestamp,
                    m.message_type,
                    m.project_path,
                    m.depth,
                    m.is_sidechain,
                    snippet(message_content_fts, 1, '**', '**', '...', ?) as context_snippet,
                    c.conversation_summary,
                    c.conversation_file,
                    c.source
                FROM messages m
                JOIN message_content_fts ON m.rowid = message_content_fts.rowid
                JOIN conversations c ON m.session_id = c.session_id
                WHERE message_content_fts.full_content MATCH ?
                  AND m.is_meta_conversation = FALSE
            """
            params = [snippet_tokens, fts_query]

        # Date filtering: use date range if provided, else days_back
        if date or since or until:
            date_sql, date_params = build_date_filter(since, until, date)
            if date_sql:
                sql += f" AND m.{date_sql}"
                params.extend(date_params)
        elif days_back:
            cutoff = (datetime.now() - timedelta(days=days_back)).isoformat()
            sql += " AND m.timestamp >= ?"
            params.append(cutoff)

        if project_path:
            sql += " AND m.project_path = ?"
            params.append(project_path)

        if repo:
            sql += " AND c.repo_root LIKE ? ESCAPE '\\'"
            params.append(f"%{_escape_like(repo)}%")

        if source:
            sql += " AND c.source = ?"
            params.append(source)

        sql += " ORDER BY m.timestamp DESC LIMIT ?"
        params.append(limit)

        try:
            cursor.execute(sql, params)
            results = cursor.fetchall()
            return [dict(row) for row in results]
        except (sqlite3.OperationalError, sqlite3.DatabaseError) as e:
            # Handle FTS corruption
            if 'fts5: missing row' in str(e) and not self._fts_rebuilt:
                print("FTS index corruption detected, rebuilding...", file=sys.stderr)
                self._rebuild_fts()
                print("FTS index rebuilt, retrying search...", file=sys.stderr)
                # Retry once
                cursor.execute(sql, params)
                results = cursor.fetchall()
                return [dict(row) for row in results]
            raise

    def get_conversation_context(
        self,
        message_uuid: str,
        depth: int = 3,
        include_children: bool = False
    ) -> Dict:
        """
        Get contextual messages around a specific message (progressive disclosure)

        Args:
            message_uuid: The message to get context for
            depth: How many parent levels to include
            include_children: Whether to include child messages (branches)

        Returns:
            Dict with the message, ancestors, and optionally children
        """
        cursor = self.conn.cursor()

        # Get the target message
        cursor.execute("""
            SELECT * FROM messages WHERE message_uuid = ?
        """, (message_uuid,))
        target = cursor.fetchone()

        if not target:
            return {"error": f"Message {message_uuid} not found"}

        target_dict = dict(target)

        # Get ancestors (walking up the tree)
        ancestors = []
        current_uuid = target_dict['parent_uuid']
        levels = 0

        while current_uuid and levels < depth:
            cursor.execute("""
                SELECT * FROM messages WHERE message_uuid = ?
            """, (current_uuid,))
            parent = cursor.fetchone()

            if not parent:
                break

            ancestors.insert(0, dict(parent))
            current_uuid = parent['parent_uuid']
            levels += 1

        # Get children (branches from this message)
        children = []
        if include_children:
            cursor.execute("""
                SELECT * FROM messages
                WHERE parent_uuid = ?
                ORDER BY timestamp ASC
            """, (message_uuid,))
            children = [dict(row) for row in cursor.fetchall()]

        # Get conversation metadata
        cursor.execute("""
            SELECT * FROM conversations WHERE session_id = ?
        """, (target_dict['session_id'],))
        conversation = dict(cursor.fetchone())

        return {
            "message": target_dict,
            "ancestors": ancestors,
            "children": children,
            "conversation": conversation,
            "context_depth": len(ancestors)
        }

    def get_conversation_tree(self, session_id: str) -> Dict:
        """
        Get the full conversation tree for a session

        Returns:
            Tree structure with all messages
        """
        cursor = self.conn.cursor()

        # Get all messages
        cursor.execute("""
            SELECT * FROM messages
            WHERE session_id = ?
            ORDER BY timestamp ASC
        """, (session_id,))
        messages = [dict(row) for row in cursor.fetchall()]

        # Get conversation metadata
        cursor.execute("""
            SELECT * FROM conversations WHERE session_id = ?
        """, (session_id,))
        conversation = cursor.fetchone()

        if not conversation:
            return {"error": f"Conversation {session_id} not found"}

        # Build tree structure
        tree = self._build_tree(messages)

        return {
            "conversation": dict(conversation),
            "tree": tree,
            "total_messages": len(messages)
        }

    def _build_tree(self, messages: List[Dict]) -> List[Dict]:
        """Build a tree structure from flat message list"""
        # Create a map of uuid -> message
        msg_map = {m['message_uuid']: {**m, 'children': []} for m in messages}

        # Build the tree
        roots = []
        for msg in msg_map.values():
            parent_uuid = msg.get('parent_uuid')
            if parent_uuid and parent_uuid in msg_map:
                msg_map[parent_uuid]['children'].append(msg)
            else:
                roots.append(msg)

        return roots

    def list_recent_conversations(
        self,
        days_back: Optional[int] = None,
        since: Optional[str] = None,
        until: Optional[str] = None,
        date: Optional[str] = None,
        limit: int = 20,
        project_path: Optional[str] = None,
        repo: Optional[str] = None,
        source: Optional[str] = None
    ) -> List[Dict]:
        """
        List recent conversations

        Args:
            days_back: Limit to last N days (default: 7 if no other filters)
            since: Start date (YYYY-MM-DD, 'yesterday', 'today')
            until: End date (YYYY-MM-DD, 'yesterday', 'today')
            date: Specific date (YYYY-MM-DD, 'yesterday', 'today')
            limit: Maximum results
            project_path: Filter by project path

        Returns:
            List of conversation metadata
        """
        # Default to 7 days if no filters provided
        if days_back is None and not (since or until or date):
            days_back = 7

        # Validate mutually exclusive date filters
        if days_back and (since or until or date):
            raise ValueError("Cannot use --days with --since/--until/--date")

        cursor = self.conn.cursor()

        sql = """
            SELECT * FROM conversations
            WHERE 1=1
        """
        params = []

        # Date filtering
        if date or since or until:
            date_sql, date_params = build_date_filter(since, until, date)
            if date_sql:
                sql += f" AND {date_sql.replace('timestamp', 'last_message_at')}"
                params.extend(date_params)
        elif days_back:
            cutoff = (datetime.now() - timedelta(days=days_back)).isoformat()
            sql += " AND last_message_at >= ?"
            params.append(cutoff)

        if project_path:
            sql += " AND project_path = ?"
            params.append(project_path)

        if repo:
            sql += " AND repo_root LIKE ? ESCAPE '\\'"
            params.append(f"%{_escape_like(repo)}%")

        if source:
            sql += " AND source = ?"
            params.append(source)

        sql += " ORDER BY last_message_at DESC LIMIT ?"
        params.append(limit)

        cursor.execute(sql, params)
        return [dict(row) for row in cursor.fetchall()]

    def get_full_message_content(self, message_uuid: str) -> Optional[str]:
        """Get the full content of a message (not just summary)"""
        cursor = self.conn.cursor()
        cursor.execute("""
            SELECT full_content FROM messages WHERE message_uuid = ?
        """, (message_uuid,))
        result = cursor.fetchone()
        return result['full_content'] if result else None

    def get_full_messages(self, uuids: List[str]) -> List[Dict]:
        """Batch fetch full content for multiple messages. Supports UUID prefixes."""
        if not uuids:
            return []

        cursor = self.conn.cursor()
        results = []

        for uuid in uuids:
            # If it's a short UUID (8 chars), use prefix matching
            if len(uuid) <= 8:
                cursor.execute("""
                    SELECT message_uuid, full_content, timestamp, message_type,
                           project_path, summary
                    FROM messages
                    WHERE message_uuid LIKE ?
                    ORDER BY timestamp
                    LIMIT 1
                """, (f"{uuid}%",))
            else:
                # Full UUID
                cursor.execute("""
                    SELECT message_uuid, full_content, timestamp, message_type,
                           project_path, summary
                    FROM messages
                    WHERE message_uuid = ?
                """, (uuid,))

            row = cursor.fetchone()
            if row:
                results.append(dict(row))

        return results

    def load_context(
        self,
        days_back: int = 1,
        project_path: Optional[str] = None,
        repo: Optional[str] = None,
        max_conversations: int = 10,
        max_messages_per_conv: int = 50
    ) -> str:
        """
        Load recent conversation context for Claude to read directly.
        Returns token-efficient formatted text.
        """
        cursor = self.conn.cursor()

        # Get recent conversations
        cutoff = (datetime.now() - timedelta(days=days_back)).isoformat()
        sql = """
            SELECT session_id, conversation_summary, project_path,
                   message_count, last_message_at
            FROM conversations
            WHERE last_message_at >= ?
                AND conversation_summary IS NOT NULL
                AND conversation_summary != 'None'
                AND message_count > 2
        """
        params = [cutoff]

        # Filter out daemon internal conversations (Haiku summarization calls)
        sql += " AND NOT (project_path LIKE '%claude/finder' AND message_count < 5)"

        if project_path:
            sql += " AND project_path = ?"
            params.append(project_path)

        if repo:
            sql += " AND repo_root LIKE ? ESCAPE '\\'"
            params.append(f"%{_escape_like(repo)}%")

        sql += " ORDER BY last_message_at DESC LIMIT ?"
        params.append(max_conversations)

        cursor.execute(sql, params)
        conversations = [dict(row) for row in cursor.fetchall()]

        if not conversations:
            return f"No conversations found in the last {days_back} day(s)."

        # Build output
        lines = [f"# Conversations (last {days_back} day{'s' if days_back != 1 else ''})\n"]

        for conv in conversations:
            # Get messages for this conversation
            cursor.execute("""
                SELECT message_uuid, timestamp, message_type, summary,
                       is_sidechain, project_path, is_tool_noise
                FROM messages
                WHERE session_id = ? AND is_tool_noise = FALSE AND is_meta_conversation = FALSE
                ORDER BY timestamp DESC
                LIMIT ?
            """, (conv['session_id'], max_messages_per_conv))

            messages = [dict(row) for row in cursor.fetchall()]
            messages.reverse()  # Chronological order

            # Format conversation block
            dt_utc = datetime.fromisoformat(conv['last_message_at'].replace('Z', '+00:00'))
            dt_local = dt_utc.astimezone()
            date_str = dt_local.strftime('%b-%d')
            time_str = format_timestamp(conv['last_message_at'], include_date=False)
            session_short = conv['session_id'][:8]

            lines.append(f"## [{session_short}] {conv['conversation_summary']}")
            lines.append(f"**{conv['message_count']} msgs** | {conv['project_path']} | {date_str} {time_str}\n")

            # Filter and format messages compactly
            # Skip tool use/result noise, only show actual conversational content
            for msg in messages:
                summary = msg['summary'] or ""

                # Skip tool-only messages (noise)
                if (not summary or
                    summary.startswith('[Tool') or
                    summary == '[Tool result]' or
                    summary.startswith('[Request interrupted') or
                    len(summary.strip()) < 10):  # Skip very short/empty summaries
                    continue

                msg_time = format_timestamp(msg['timestamp'], include_date=False)
                icon = "👤" if msg['message_type'] == 'user' else "🤖"
                branch = "🌿 " if msg['is_sidechain'] else ""
                uuid_short = msg['message_uuid'][:8]

                lines.append(f"{icon} {msg_time} `{uuid_short}` {branch}{summary}")

            lines.append("")  # Blank line between conversations

        return "\n".join(lines)

    def _rebuild_fts(self):
        """Rebuild FTS index from scratch"""
        cursor = self.conn.cursor()

        # Use FTS5 rebuild command - this is the proper way to rebuild content tables
        cursor.execute("INSERT INTO message_content_fts(message_content_fts) VALUES('rebuild')")

        self.conn.commit()
        self._fts_rebuilt = True

    def _validate_fts(self) -> bool:
        """Check if FTS is in sync with messages table. Returns True if valid."""
        cursor = self.conn.cursor()

        try:
            # FTS5 integrity check
            cursor.execute("INSERT INTO message_content_fts(message_content_fts) VALUES('integrity-check')")
            return True
        except sqlite3.Error:
            return False

    def close(self):
        """Close database connection"""
        self.conn.close()


