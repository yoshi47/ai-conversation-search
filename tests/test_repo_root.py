#!/usr/bin/env python3
"""Tests for repo_root resolution and filtering"""

import os
import pytest
import sqlite3
import subprocess
import tempfile
from pathlib import Path
from unittest.mock import patch, MagicMock

from conversation_search.core.git_utils import resolve_repo_root


class TestResolveRepoRoot:
    """Test resolve_repo_root function"""

    def test_normal_git_repo(self, tmp_path):
        """resolve_repo_root returns the repo root for a normal git repo"""
        subprocess.run(["git", "init", str(tmp_path)], capture_output=True)
        result = resolve_repo_root(str(tmp_path))
        assert result == str(tmp_path)

    def test_subdirectory_of_repo(self, tmp_path, monkeypatch):
        """resolve_repo_root returns repo root from a subdirectory"""
        repo = tmp_path / "myrepo"
        repo.mkdir()
        # Set ceiling to prevent resolving to any parent git repo
        monkeypatch.setenv("GIT_CEILING_DIRECTORIES", str(tmp_path))
        subprocess.run(["git", "init", str(repo)], capture_output=True)
        subdir = repo / "src" / "deep"
        subdir.mkdir(parents=True)
        result = resolve_repo_root(str(subdir))
        assert result == str(repo)

    def test_non_git_directory(self, tmp_path):
        """resolve_repo_root returns None for non-git directories"""
        non_git = tmp_path / "not-a-repo"
        non_git.mkdir()
        result = resolve_repo_root(str(non_git))
        assert result is None

    def test_nonexistent_path(self):
        """resolve_repo_root returns None for nonexistent paths"""
        result = resolve_repo_root("/nonexistent/path/that/does/not/exist")
        assert result is None

    def test_worktree_resolves_to_main_repo(self, tmp_path):
        """resolve_repo_root resolves worktree to main repo root"""
        # Create main repo
        main_repo = tmp_path / "main"
        main_repo.mkdir()
        subprocess.run(["git", "init", str(main_repo)], capture_output=True)
        subprocess.run(
            ["git", "-C", str(main_repo), "commit", "--allow-empty", "-m", "init"],
            capture_output=True
        )

        # Create worktree
        worktree = tmp_path / "worktree"
        result = subprocess.run(
            ["git", "-C", str(main_repo), "worktree", "add", str(worktree), "-b", "wt-branch"],
            capture_output=True
        )
        if result.returncode != 0:
            pytest.skip("git worktree not supported")

        # Resolve from worktree should return main repo
        resolved = resolve_repo_root(str(worktree))
        assert resolved == str(main_repo)


class TestRepoRootFiltering:
    """Test --repo filtering in search and list"""

    @pytest.fixture
    def db_with_repo_data(self):
        """Create temporary database with repo_root test data"""
        with tempfile.NamedTemporaryFile(mode='w', suffix='.db', delete=False) as f:
            db_path = f.name

        conn = sqlite3.connect(db_path)
        conn.execute("PRAGMA journal_mode=WAL")

        conn.executescript("""
            CREATE TABLE messages (
                message_uuid TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                parent_uuid TEXT,
                is_sidechain BOOLEAN DEFAULT FALSE,
                depth INTEGER DEFAULT 0,
                timestamp TEXT NOT NULL,
                message_type TEXT NOT NULL,
                project_path TEXT,
                conversation_file TEXT,
                summary TEXT,
                full_content TEXT NOT NULL,
                is_summarized BOOLEAN DEFAULT FALSE,
                is_tool_noise BOOLEAN DEFAULT FALSE,
                is_meta_conversation BOOLEAN DEFAULT FALSE,
                summary_method TEXT,
                indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE conversations (
                session_id TEXT PRIMARY KEY,
                project_path TEXT,
                repo_root TEXT,
                conversation_file TEXT,
                root_message_uuid TEXT,
                leaf_message_uuid TEXT,
                conversation_summary TEXT,
                first_message_at TEXT,
                last_message_at TEXT,
                message_count INTEGER DEFAULT 0,
                indexed_at TEXT DEFAULT CURRENT_TIMESTAMP
            );

            CREATE VIRTUAL TABLE message_content_fts USING fts5(
                message_uuid UNINDEXED,
                full_content,
                content='messages',
                content_rowid='rowid'
            );

            CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO message_content_fts(rowid, message_uuid, full_content)
                VALUES (new.rowid, new.message_uuid, new.full_content);
            END;
        """)

        # Insert test data: two repos
        conn.execute("""
            INSERT INTO conversations (session_id, project_path, repo_root,
                conversation_summary, first_message_at, last_message_at, message_count)
            VALUES ('sess-1', '/Users/test/code/project-a', '/Users/test/code/project-a',
                'Working on project A', '2026-03-01T00:00:00Z', '2026-03-01T01:00:00Z', 5)
        """)
        conn.execute("""
            INSERT INTO conversations (session_id, project_path, repo_root,
                conversation_summary, first_message_at, last_message_at, message_count)
            VALUES ('sess-2', '/Users/test/code/project-a/subdir', '/Users/test/code/project-a',
                'Subdir work in A', '2026-03-01T02:00:00Z', '2026-03-01T03:00:00Z', 3)
        """)
        conn.execute("""
            INSERT INTO conversations (session_id, project_path, repo_root,
                conversation_summary, first_message_at, last_message_at, message_count)
            VALUES ('sess-3', '/Users/test/code/project-b', '/Users/test/code/project-b',
                'Working on project B', '2026-03-01T04:00:00Z', '2026-03-01T05:00:00Z', 4)
        """)

        # Insert messages for each session
        for i, (sess, proj, content) in enumerate([
            ('sess-1', '/Users/test/code/project-a', 'message in project A root'),
            ('sess-2', '/Users/test/code/project-a/subdir', 'message in project A subdir'),
            ('sess-3', '/Users/test/code/project-b', 'message in project B'),
        ]):
            conn.execute("""
                INSERT INTO messages (message_uuid, session_id, timestamp, message_type,
                    project_path, full_content, is_meta_conversation)
                VALUES (?, ?, '2026-03-01T00:00:00Z', 'user', ?, ?, FALSE)
            """, (f'uuid-{i}', sess, proj, content))

        conn.commit()
        conn.close()

        yield db_path
        os.unlink(db_path)

    def test_list_with_repo_filter(self, db_with_repo_data):
        """--repo filters conversations by repo_root"""
        from conversation_search.core.search import ConversationSearch
        search = ConversationSearch(db_path=db_with_repo_data)

        # Filter by project-a repo
        results = search.list_recent_conversations(
            days_back=None, since='2026-03-01', until='2026-03-02',
            repo='project-a'
        )
        assert len(results) == 2
        for r in results:
            assert 'project-a' in r['repo_root']

        # Filter by project-b repo
        results = search.list_recent_conversations(
            days_back=None, since='2026-03-01', until='2026-03-02',
            repo='project-b'
        )
        assert len(results) == 1
        assert results[0]['session_id'] == 'sess-3'

        search.close()

    def test_search_with_repo_filter(self, db_with_repo_data):
        """search_conversations respects --repo filter"""
        from conversation_search.core.search import ConversationSearch
        search = ConversationSearch(db_path=db_with_repo_data)

        # Search across all repos
        results = search.search_conversations(query='message', days_back=None)
        assert len(results) == 3

        # Search with repo filter
        results = search.search_conversations(query='message', days_back=None, repo='project-a')
        assert len(results) == 2

        results = search.search_conversations(query='message', days_back=None, repo='project-b')
        assert len(results) == 1

        search.close()

    def test_repo_filter_no_match(self, db_with_repo_data):
        """--repo with no matching repo returns empty results"""
        from conversation_search.core.search import ConversationSearch
        search = ConversationSearch(db_path=db_with_repo_data)

        results = search.list_recent_conversations(
            days_back=None, since='2026-03-01', until='2026-03-02',
            repo='nonexistent-project'
        )
        assert len(results) == 0

        search.close()


class TestRepoRootCache:
    """Test caching behavior in the indexer"""

    def test_cache_stores_and_retrieves(self, tmp_path):
        """Resolved repo roots are cached in the database"""
        from conversation_search.core.indexer import ConversationIndexer

        # Create a git repo
        repo = tmp_path / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", str(repo)], capture_output=True)

        db_path = tmp_path / "test.db"
        indexer = ConversationIndexer(db_path=str(db_path), quiet=True)

        # Mock _decode_project_dir_name to return our test repo path
        with patch.object(indexer, '_decode_project_dir_name', return_value=str(repo)):
            result1 = indexer._resolve_repo_root('test/project', '/fake/projects/test-hash/conv.jsonl')
            assert result1 == str(repo)

        # Check cache was populated
        cursor = indexer.conn.cursor()
        cursor.execute("SELECT repo_root FROM repo_root_cache WHERE project_path = ?", ('test/project',))
        cached = cursor.fetchone()
        assert cached is not None
        assert cached['repo_root'] == str(repo)

        # Second call uses cache (decode not called again)
        with patch.object(indexer, '_decode_project_dir_name', return_value=None):
            result2 = indexer._resolve_repo_root('test/project', '/fake/projects/test-hash/conv.jsonl')
            assert result2 == str(repo)  # Still returns cached value

        indexer.close()

    def test_none_result_is_not_cached(self, tmp_path):
        """Unresolvable paths are NOT cached so transient failures can be retried"""
        from conversation_search.core.indexer import ConversationIndexer

        db_path = tmp_path / "test.db"
        indexer = ConversationIndexer(db_path=str(db_path), quiet=True)

        # Both strategies return None
        with patch.object(indexer, '_decode_project_dir_name', return_value=None):
            result = indexer._resolve_repo_root('nonexistent/path', '/fake/conv.jsonl')
            assert result is None

        # Verify it was NOT cached
        cursor = indexer.conn.cursor()
        cursor.execute("SELECT repo_root FROM repo_root_cache WHERE project_path = ?", ('nonexistent/path',))
        cached = cursor.fetchone()
        assert cached is None

        indexer.close()
