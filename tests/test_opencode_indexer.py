#!/usr/bin/env python3
"""Tests for OpenCode conversation indexer."""

import json
import sqlite3
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from conversation_search.core.opencode_indexer import OpenCodeIndexer, OC_PREFIX


@pytest.fixture
def opencode_db(tmp_path):
    """Create a minimal OpenCode database with test data."""
    db_path = tmp_path / "opencode.db"
    conn = sqlite3.connect(str(db_path))

    conn.executescript("""
        CREATE TABLE project (
            id TEXT PRIMARY KEY,
            worktree TEXT NOT NULL,
            vcs TEXT,
            name TEXT,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            sandboxes TEXT NOT NULL
        );

        CREATE TABLE session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            parent_id TEXT,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT NOT NULL,
            version TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            FOREIGN KEY (project_id) REFERENCES project(id)
        );

        CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL,
            FOREIGN KEY (session_id) REFERENCES session(id)
        );

        CREATE TABLE part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL,
            FOREIGN KEY (message_id) REFERENCES message(id)
        );
    """)

    # Insert test data
    conn.execute("""
        INSERT INTO project VALUES (
            'proj1', '/Users/test/code/myproject', 'git', 'myproject',
            1700000000000, 1700000000000, '[]'
        )
    """)

    conn.execute("""
        INSERT INTO session VALUES (
            'sess1', 'proj1', NULL, 'test-session', '/Users/test/code/myproject',
            'Test Session Title', '1.0',
            1700000000000, 1700000100000
        )
    """)

    # User message
    conn.execute("""
        INSERT INTO message VALUES (
            'msg1', 'sess1', 1700000000000, 1700000000000, ?
        )
    """, (json.dumps({"role": "user"}),))

    # Assistant message
    conn.execute("""
        INSERT INTO message VALUES (
            'msg2', 'sess1', 1700000050000, 1700000050000, ?
        )
    """, (json.dumps({"role": "assistant"}),))

    # Parts for user message
    conn.execute("""
        INSERT INTO part VALUES (
            'part1', 'msg1', 'sess1', 1700000000000, 1700000000000, ?
        )
    """, (json.dumps({"type": "text", "text": "How do I use chezmoi?"}),))

    # Parts for assistant message
    conn.execute("""
        INSERT INTO part VALUES (
            'part2', 'msg2', 'sess1', 1700000050000, 1700000050000, ?
        )
    """, (json.dumps({"type": "text", "text": "Chezmoi is a dotfile manager. Here's how to use it."}),))

    conn.execute("""
        INSERT INTO part VALUES (
            'part3', 'msg2', 'sess1', 1700000051000, 1700000051000, ?
        )
    """, (json.dumps({
        "type": "tool",
        "tool": "bash",
        "callID": "call1",
        "state": {
            "status": "completed",
            "input": {"command": "chezmoi status"},
            "output": "some output"
        }
    }),))

    # Step-start and step-finish parts (should be skipped)
    conn.execute("""
        INSERT INTO part VALUES (
            'part4', 'msg2', 'sess1', 1700000049000, 1700000049000, ?
        )
    """, (json.dumps({"type": "step-start", "snapshot": "abc123"}),))

    conn.execute("""
        INSERT INTO part VALUES (
            'part5', 'msg2', 'sess1', 1700000052000, 1700000052000, ?
        )
    """, (json.dumps({"type": "step-finish", "reason": "stop"}),))

    conn.commit()
    conn.close()
    return db_path


@pytest.fixture
def search_db(tmp_path):
    """Create a search database with the schema."""
    from importlib.resources import files
    db_path = tmp_path / "search.db"
    conn = sqlite3.connect(str(db_path))
    schema_sql = files('conversation_search.data').joinpath('schema.sql').read_text()
    conn.executescript(schema_sql)
    conn.commit()
    conn.close()
    return db_path


class TestOpenCodeIndexer:
    def test_scan_and_index_basic(self, opencode_db, search_db):
        """Test basic indexing of OpenCode sessions."""
        with patch('conversation_search.core.opencode_indexer.resolve_repo_root', return_value='/Users/test/code/myproject'):
            indexer = OpenCodeIndexer(
                search_db_path=str(search_db),
                opencode_db_path=str(opencode_db),
                quiet=True
            )
            count = indexer.scan_and_index(days_back=9999)

        assert count == 1

        # Verify data in search DB
        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row

        # Check conversation
        conv = conn.execute("SELECT * FROM conversations WHERE session_id = ?",
                           (OC_PREFIX + 'sess1',)).fetchone()
        assert conv is not None
        assert conv['conversation_summary'] == 'Test Session Title'
        assert conv['source'] == 'opencode'
        assert conv['project_path'] == '/Users/test/code/myproject'
        assert conv['repo_root'] == '/Users/test/code/myproject'

        # Check messages
        messages = conn.execute("SELECT * FROM messages WHERE session_id = ? ORDER BY timestamp",
                               (OC_PREFIX + 'sess1',)).fetchall()
        assert len(messages) == 2

        # User message
        assert messages[0]['message_type'] == 'user'
        assert 'chezmoi' in messages[0]['full_content']

        # Assistant message with tool
        assert messages[1]['message_type'] == 'assistant'
        assert 'Chezmoi is a dotfile manager' in messages[1]['full_content']
        assert '[Tool: bash]' in messages[1]['full_content']
        assert 'chezmoi status' in messages[1]['full_content']

        conn.close()

    def test_fts_search_opencode_content(self, opencode_db, search_db):
        """Test that OpenCode content is searchable via FTS."""
        with patch('conversation_search.core.opencode_indexer.resolve_repo_root', return_value=None):
            indexer = OpenCodeIndexer(
                search_db_path=str(search_db),
                opencode_db_path=str(opencode_db),
                quiet=True
            )
            indexer.scan_and_index(days_back=9999)

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row

        # Search for content
        results = conn.execute("""
            SELECT m.message_uuid, m.full_content
            FROM messages m
            JOIN message_content_fts ON m.rowid = message_content_fts.rowid
            WHERE message_content_fts.full_content MATCH 'chezmoi'
        """).fetchall()

        assert len(results) >= 1
        conn.close()

    def test_incremental_sync(self, opencode_db, search_db):
        """Test that incremental sync only processes new sessions."""
        with patch('conversation_search.core.opencode_indexer.resolve_repo_root', return_value=None):
            indexer = OpenCodeIndexer(
                search_db_path=str(search_db),
                opencode_db_path=str(opencode_db),
                quiet=True
            )

            # First sync
            count1 = indexer.scan_and_index(days_back=9999)
            assert count1 == 1

            # Second sync without changes - should find 0 new
            count2 = indexer.scan_and_index(days_back=None)
            assert count2 == 0

    def test_session_update_reindexes(self, opencode_db, search_db):
        """Test that updated sessions get reindexed."""
        with patch('conversation_search.core.opencode_indexer.resolve_repo_root', return_value=None):
            indexer = OpenCodeIndexer(
                search_db_path=str(search_db),
                opencode_db_path=str(opencode_db),
                quiet=True
            )
            indexer.scan_and_index(days_back=9999)

        # Update session time
        oc_conn = sqlite3.connect(str(opencode_db))
        oc_conn.execute("UPDATE session SET time_updated = 1700000200000 WHERE id = 'sess1'")
        oc_conn.commit()
        oc_conn.close()

        with patch('conversation_search.core.opencode_indexer.resolve_repo_root', return_value=None):
            indexer = OpenCodeIndexer(
                search_db_path=str(search_db),
                opencode_db_path=str(opencode_db),
                quiet=True
            )
            count = indexer.scan_and_index(days_back=None)
            assert count == 1

    def test_missing_opencode_db(self, search_db, tmp_path):
        """Test graceful handling of missing OpenCode DB."""
        indexer = OpenCodeIndexer(
            search_db_path=str(search_db),
            opencode_db_path=str(tmp_path / "nonexistent.db"),
            quiet=True
        )
        count = indexer.scan_and_index()
        assert count == 0

    def test_session_id_prefix(self, opencode_db, search_db):
        """Test that session IDs are prefixed with 'oc:' to avoid collisions."""
        with patch('conversation_search.core.opencode_indexer.resolve_repo_root', return_value=None):
            indexer = OpenCodeIndexer(
                search_db_path=str(search_db),
                opencode_db_path=str(opencode_db),
                quiet=True
            )
            indexer.scan_and_index(days_back=9999)

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row

        conv = conn.execute("SELECT session_id FROM conversations WHERE source = 'opencode'").fetchone()
        assert conv['session_id'].startswith('oc:')

        msg = conn.execute("SELECT message_uuid FROM messages WHERE session_id LIKE 'oc:%'").fetchone()
        assert msg['message_uuid'].startswith('oc:')

        conn.close()

    def test_build_message_content_skips_non_text(self):
        """Test that step-start, step-finish, reasoning, compaction parts are skipped."""
        indexer = OpenCodeIndexer(quiet=True)

        # Create mock part rows
        class MockRow:
            def __init__(self, data):
                self._data = data
            def __getitem__(self, key):
                return self._data[key]

        parts = [
            MockRow({'data': json.dumps({"type": "step-start", "snapshot": "abc"})}),
            MockRow({'data': json.dumps({"type": "text", "text": "Hello world"})}),
            MockRow({'data': json.dumps({"type": "reasoning", "text": "thinking..."})}),
            MockRow({'data': json.dumps({"type": "step-finish", "reason": "stop"})}),
            MockRow({'data': json.dumps({"type": "compaction", "text": "compacted"})}),
            MockRow({'data': json.dumps({"type": "file", "path": "test.py"})}),
        ]

        content = indexer._build_message_content(parts)
        assert "Hello world" in content
        assert "thinking" not in content
        assert "compacted" not in content
        assert "[File change]" in content

    def test_epoch_ms_to_iso(self):
        """Test timestamp conversion."""
        indexer = OpenCodeIndexer(quiet=True)
        result = indexer._epoch_ms_to_iso(1700000000000)
        assert result.startswith('2023-11-14T')
        assert result.endswith('Z')
