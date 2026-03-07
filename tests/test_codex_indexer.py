#!/usr/bin/env python3
"""Tests for Codex CLI conversation indexer."""

import json
import sqlite3
import tempfile
from datetime import datetime, timedelta
from pathlib import Path
from unittest.mock import patch

import pytest

from conversation_search.core.codex_indexer import CodexIndexer, CX_PREFIX


def _make_session_file(path: Path, events: list[dict], meta_overrides: dict = None):
    """Helper to create a Codex session JSONL file."""
    meta = {
        "timestamp": "2025-11-04T03:05:46.251Z",
        "type": "session_meta",
        "payload": {
            "id": "019a4cd3-e3a2-7f73-9181-4293b1a25f23",
            "timestamp": "2025-11-04T03:05:46.146Z",
            "cwd": "/Users/test/code/myproject",
            "originator": "codex_cli_rs",
            "cli_version": "0.53.0",
            "instructions": "",
            "source": "cli",
            "model_provider": "openai",
            "git": {
                "commit_hash": "abc123",
                "branch": "main",
                "repository_url": "git@github.com:test/myproject.git"
            }
        }
    }
    if meta_overrides:
        meta["payload"].update(meta_overrides)

    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [json.dumps(meta)]
    for event in events:
        lines.append(json.dumps(event))
    path.write_text('\n'.join(lines))


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


@pytest.fixture
def codex_sessions(tmp_path):
    """Create a Codex sessions directory with test data."""
    sessions_dir = tmp_path / "codex_sessions" / "2025" / "11" / "04"
    session_file = sessions_dir / "rollout-2025-11-04T12-05-46-019a4cd3-e3a2-7f73-9181-4293b1a25f23.jsonl"

    events = [
        {
            "timestamp": "2025-11-04T03:06:05.428Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "How do I use pytest fixtures?", "images": []}
        },
        {
            "timestamp": "2025-11-04T03:06:06.609Z",
            "type": "event_msg",
            "payload": {"type": "token_count", "info": None, "rate_limits": {"primary": None}}
        },
        {
            "timestamp": "2025-11-04T03:06:16.346Z",
            "type": "event_msg",
            "payload": {"type": "agent_message", "message": "Pytest fixtures are reusable test components. Here's how to use them."}
        },
        {
            "timestamp": "2025-11-04T03:06:17.472Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "shell",
                "arguments": json.dumps({"command": ["bash", "-lc", "cat conftest.py"]}),
                "call_id": "call_123"
            }
        },
        {
            "timestamp": "2025-11-04T03:06:17.472Z",
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_123",
                "output": json.dumps({"output": "import pytest\n\n@pytest.fixture\ndef sample():\n    return 42", "metadata": {"exit_code": 0}})
            }
        },
    ]

    _make_session_file(session_file, events)
    return tmp_path / "codex_sessions"


class TestCodexIndexer:
    def test_scan_and_index_basic(self, codex_sessions, search_db):
        """Test basic indexing of Codex sessions."""
        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value='/Users/test/code/myproject'):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(codex_sessions),
                quiet=True
            )
            count = indexer.scan_and_index(days_back=9999)

        assert count == 1

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row

        conv = conn.execute("SELECT * FROM conversations WHERE source = 'codex'").fetchone()
        assert conv is not None
        assert conv['conversation_summary'] == 'How do I use pytest fixtures?'
        assert conv['source'] == 'codex'
        assert conv['project_path'] == '/Users/test/code/myproject'
        assert conv['repo_root'] == '/Users/test/code/myproject'

        messages = conn.execute(
            "SELECT * FROM messages WHERE session_id = ? ORDER BY depth",
            (conv['session_id'],)
        ).fetchall()
        assert len(messages) >= 2

        # User message
        assert messages[0]['message_type'] == 'user'
        assert 'pytest fixtures' in messages[0]['full_content']

        conn.close()

    def test_fts_search(self, codex_sessions, search_db):
        """Test that Codex content is searchable via FTS."""
        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(codex_sessions),
                quiet=True
            )
            indexer.scan_and_index(days_back=9999)

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row

        results = conn.execute("""
            SELECT m.message_uuid, m.full_content
            FROM messages m
            JOIN message_content_fts ON m.rowid = message_content_fts.rowid
            WHERE message_content_fts.full_content MATCH 'pytest'
        """).fetchall()

        assert len(results) >= 1
        conn.close()

    def test_incremental_sync(self, codex_sessions, search_db):
        """Test that unchanged files are not re-indexed."""
        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(codex_sessions),
                quiet=True
            )

            count1 = indexer.scan_and_index(days_back=9999)
            assert count1 == 1

            count2 = indexer.scan_and_index(days_back=9999)
            assert count2 == 0

    def test_file_update_reindexes(self, codex_sessions, search_db):
        """Test that modified files get re-indexed."""
        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(codex_sessions),
                quiet=True
            )
            indexer.scan_and_index(days_back=9999)

        # Modify the file (append a new event)
        session_file = list(codex_sessions.rglob("*.jsonl"))[0]
        with open(session_file, 'a') as f:
            f.write('\n' + json.dumps({
                "timestamp": "2025-11-04T03:07:00.000Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Thanks!", "images": []}
            }))

        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(codex_sessions),
                quiet=True
            )
            count = indexer.scan_and_index(days_back=9999)
            assert count == 1

    def test_missing_sessions_dir(self, search_db, tmp_path):
        """Test graceful handling of missing sessions directory."""
        indexer = CodexIndexer(
            search_db_path=str(search_db),
            sessions_dir=str(tmp_path / "nonexistent"),
            quiet=True
        )
        count = indexer.scan_and_index()
        assert count == 0

    def test_session_id_prefix(self, codex_sessions, search_db):
        """Test that session IDs are prefixed with 'codex:' to avoid collisions."""
        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(codex_sessions),
                quiet=True
            )
            indexer.scan_and_index(days_back=9999)

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row

        conv = conn.execute("SELECT session_id FROM conversations WHERE source = 'codex'").fetchone()
        assert conv['session_id'].startswith('codex:')

        msg = conn.execute("SELECT message_uuid FROM messages WHERE session_id LIKE 'codex:%'").fetchone()
        assert msg['message_uuid'].startswith('codex:')

        conn.close()

    def test_tool_call_indexed(self, codex_sessions, search_db):
        """Test that tool calls are indexed with tool name and command."""
        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(codex_sessions),
                quiet=True
            )
            indexer.scan_and_index(days_back=9999)

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row

        # Find assistant messages containing tool calls
        messages = conn.execute("""
            SELECT full_content FROM messages
            WHERE session_id LIKE 'codex:%' AND message_type = 'assistant'
        """).fetchall()

        tool_contents = [m['full_content'] for m in messages]
        all_content = '\n'.join(tool_contents)
        assert '[Tool: shell]' in all_content
        assert 'cat conftest.py' in all_content

        conn.close()

    def test_developer_messages_skipped(self, tmp_path, search_db):
        """Test that developer/user context messages from response_item are skipped."""
        sessions_dir = tmp_path / "sessions" / "2025" / "11" / "04"
        session_file = sessions_dir / "rollout-2025-11-04T00-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"

        events = [
            # Developer context message (should be skipped)
            {
                "timestamp": "2025-11-04T00:00:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "System instructions here"}]
                }
            },
            # User context message (should be skipped)
            {
                "timestamp": "2025-11-04T00:00:02.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "<environment_context>stuff</environment_context>"}]
                }
            },
            # Encrypted reasoning (should be skipped)
            {
                "timestamp": "2025-11-04T00:00:03.000Z",
                "type": "response_item",
                "payload": {
                    "type": "reasoning",
                    "summary": [],
                    "content": None,
                    "encrypted_content": "gAAAAA..."
                }
            },
            # Actual user message
            {
                "timestamp": "2025-11-04T00:00:04.000Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Hello world", "images": []}
            },
            # Actual agent message
            {
                "timestamp": "2025-11-04T00:00:05.000Z",
                "type": "event_msg",
                "payload": {"type": "agent_message", "message": "Hi there!"}
            },
        ]

        _make_session_file(session_file, events, {"id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"})

        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(tmp_path / "sessions"),
                quiet=True
            )
            indexer.scan_and_index(days_back=9999)

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row

        messages = conn.execute(
            "SELECT * FROM messages WHERE session_id LIKE 'codex:%' ORDER BY depth"
        ).fetchall()

        # Only real user and assistant messages should be indexed
        assert len(messages) == 2
        assert messages[0]['message_type'] == 'user'
        assert 'Hello world' in messages[0]['full_content']
        assert messages[1]['message_type'] == 'assistant'
        assert 'Hi there!' in messages[1]['full_content']

        # Developer/context messages should NOT be present
        all_content = ' '.join(m['full_content'] for m in messages)
        assert 'System instructions' not in all_content
        assert 'environment_context' not in all_content

        conn.close()

    def test_days_back_filtering(self, tmp_path, search_db):
        """Test that days_back parameter filters by date directories."""
        today = datetime.now()
        old_date = today - timedelta(days=30)

        # Create a session in today's directory
        today_dir = tmp_path / "sessions" / str(today.year) / f"{today.month:02d}" / f"{today.day:02d}"
        today_file = today_dir / "rollout-today-aaaaaaaa-bbbb-cccc-dddd-111111111111.jsonl"
        _make_session_file(today_file, [
            {"timestamp": today.isoformat() + "Z", "type": "event_msg",
             "payload": {"type": "user_message", "message": "Today's message", "images": []}}
        ], {"id": "aaaaaaaa-bbbb-cccc-dddd-111111111111"})

        # Create a session in an old directory
        old_dir = tmp_path / "sessions" / str(old_date.year) / f"{old_date.month:02d}" / f"{old_date.day:02d}"
        old_file = old_dir / "rollout-old-aaaaaaaa-bbbb-cccc-dddd-222222222222.jsonl"
        _make_session_file(old_file, [
            {"timestamp": old_date.isoformat() + "Z", "type": "event_msg",
             "payload": {"type": "user_message", "message": "Old message", "images": []}}
        ], {"id": "aaaaaaaa-bbbb-cccc-dddd-222222222222"})

        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(tmp_path / "sessions"),
                quiet=True
            )
            # Only index last 7 days
            count = indexer.scan_and_index(days_back=7)
            assert count == 1

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row
        messages = conn.execute("SELECT full_content FROM messages WHERE session_id LIKE 'codex:%'").fetchall()
        all_content = ' '.join(m['full_content'] for m in messages)
        assert "Today's message" in all_content
        assert "Old message" not in all_content
        conn.close()

    def test_extract_uuid_from_filename(self):
        """Test UUID extraction from various filename formats."""
        indexer = CodexIndexer(quiet=True)

        # Standard format
        assert indexer._extract_uuid_from_filename(
            "rollout-2025-11-04T12-05-46-019a4cd3-e3a2-7f73-9181-4293b1a25f23.jsonl"
        ) == "019a4cd3-e3a2-7f73-9181-4293b1a25f23"

        # No UUID in filename
        result = indexer._extract_uuid_from_filename("random-file.jsonl")
        assert result == "random-file"

    def test_corrupt_file_does_not_block_others(self, tmp_path, search_db):
        """Test that a corrupt file doesn't prevent other files from being indexed."""
        sessions_dir = tmp_path / "sessions" / "2025" / "11" / "04"
        sessions_dir.mkdir(parents=True)

        # Create a corrupt file
        corrupt_file = sessions_dir / "rollout-corrupt-aaaaaaaa-bbbb-cccc-dddd-111111111111.jsonl"
        corrupt_file.write_text("not valid json at all\nmore garbage")

        # Create a valid file
        valid_file = sessions_dir / "rollout-valid-aaaaaaaa-bbbb-cccc-dddd-222222222222.jsonl"
        _make_session_file(valid_file, [
            {"timestamp": "2025-11-04T00:00:01.000Z", "type": "event_msg",
             "payload": {"type": "user_message", "message": "Valid message", "images": []}}
        ], {"id": "aaaaaaaa-bbbb-cccc-dddd-222222222222"})

        with patch('conversation_search.core.codex_indexer.resolve_repo_root', return_value=None):
            indexer = CodexIndexer(
                search_db_path=str(search_db),
                sessions_dir=str(tmp_path / "sessions"),
                quiet=True
            )
            count = indexer.scan_and_index(days_back=9999)
            # The valid file should still be indexed
            assert count == 1

        conn = sqlite3.connect(str(search_db))
        conn.row_factory = sqlite3.Row
        messages = conn.execute("SELECT full_content FROM messages WHERE session_id LIKE 'codex:%'").fetchall()
        assert any('Valid message' in m['full_content'] for m in messages)
        conn.close()
