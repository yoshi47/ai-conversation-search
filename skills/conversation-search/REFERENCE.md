# Conversation Search - Technical Reference

## Supported Sources

The tool indexes conversations from multiple AI coding assistants:

| Source | Value | Data Location | Session Prefix |
|--------|-------|--------------|---------------|
| Claude Code | `claude_code` | `~/.claude/projects/` (JSONL) | *(none)* |
| OpenCode | `opencode` | `~/.local/share/opencode/opencode.db` (SQLite) | `oc:` |
| Codex CLI | `codex` | `~/.codex/sessions/` (JSONL) | `codex:` |

All sources are automatically detected and indexed together.

## Complete Command Reference

### ai-conversation-search init

Initialize the database and perform initial indexing of all detected sources.

```bash
ai-conversation-search init [--days DAYS] [--no-extract] [--force]
```

**Options:**
- `--days DAYS`: Index last N days of conversations (default: 7)
- `--no-extract`: Skip smart extraction, store only raw content
- `--force`: Reinitialize existing database

**What it does:**
1. Creates `~/.conversation-search/index.db` SQLite database
2. Scans all supported sources (Claude Code, OpenCode, Codex CLI)
3. Parses conversation formats (JSONL and SQLite)
4. Extracts searchable content using smart hybrid extraction (instant, no AI)
5. Builds FTS5 search index

**Example:**
```bash
# Initialize with last 30 days
ai-conversation-search init --days 30

# Store only raw content (skip extraction)
ai-conversation-search init --no-extract
```

---

### ai-conversation-search search

Search conversations using full-text search on smart-extracted content.

```bash
ai-conversation-search search QUERY [--days DAYS] [--project PROJECT] [--source SOURCE] [--limit LIMIT] [--content] [--json]
```

**Arguments:**
- `QUERY`: Search query (supports FTS5 syntax)

**Options:**
- `--days DAYS`: Limit to last N days
- `--project PROJECT`: Filter by project path
- `--source SOURCE`: Filter by source (`claude_code`, `opencode`, `codex`)
- `--limit LIMIT`: Max results (default: 20)
- `--content`: Show full message content instead of summaries
- `--json`: Output as JSON

**Search Syntax:**
- Simple: `authentication bug`
- Multiple terms: `react hooks useEffect` (implicit AND)
- Phrases: `"exact phrase"`
- Operators: `auth AND bug`, `react OR vue`

**Examples:**
```bash
# Basic search
ai-conversation-search search "authentication"

# Time-scoped search
ai-conversation-search search "database" --days 30

# Project-specific search
ai-conversation-search search "api" --project /home/user/myapp

# Get JSON output (for programmatic use)
ai-conversation-search search "hooks" --json
```

---

### ai-conversation-search context

Get conversation context around a specific message.

```bash
ai-conversation-search context MESSAGE_UUID [--depth DEPTH] [--content] [--json]
```

**Arguments:**
- `MESSAGE_UUID`: Message UUID from search results

**Options:**
- `--depth DEPTH`: How many parent levels to show (default: 3)
- `--content`: Show full content instead of summaries
- `--json`: Output as JSON

**What it returns:**
- Parent messages (conversation history leading to this message)
- Target message
- Child messages (responses to this message)

**Example:**
```bash
# Get context for a message
ai-conversation-search context abc-123-def --depth 5

# With full content
ai-conversation-search context abc-123-def --content --json
```

---

### ai-conversation-search list

List recent conversations.

```bash
ai-conversation-search list [--days DAYS] [--limit LIMIT] [--source SOURCE] [--json]
```

**Options:**
- `--days DAYS`: Show conversations from last N days (default: 7)
- `--limit LIMIT`: Max conversations to show (default: 20)
- `--source SOURCE`: Filter by source (`claude_code`, `opencode`, `codex`)
- `--json`: Output as JSON

**Example:**
```bash
# List last week's conversations
ai-conversation-search list --days 7

# List last 50 conversations
ai-conversation-search list --limit 50 --json
```

---

### ai-conversation-search tree

Show the conversation tree structure for a session.

```bash
ai-conversation-search tree SESSION_ID [--json]
```

**Arguments:**
- `SESSION_ID`: Session ID from list or search results

**Options:**
- `--json`: Output as JSON

**Use case:** Visualize conversation branching and checkpoint structure.

**Example:**
```bash
ai-conversation-search tree session-abc-123
```

---

### ai-conversation-search index

JIT index conversations (instant, no AI calls). The skill runs this before every search.

```bash
ai-conversation-search index [--days DAYS] [--all] [--no-extract]
```

**Options:**
- `--days DAYS`: Index last N days (default: 1)
- `--all`: Index all conversations
- `--no-extract`: Skip smart extraction

**What it does:**
- Scans for new/modified conversations
- Extracts searchable content (instant, deterministic)
- Updates FTS5 search index
- Typically completes in <1 second for recent conversations

**Example:**
```bash
# JIT index last week (typical usage)
ai-conversation-search index --days 7

# Reindex everything
ai-conversation-search index --all
```

---

## Database Schema

**Location:** `~/.conversation-search/index.db`

**Tables:**
- `messages`: Individual messages with summaries and tree structure
- `conversations`: Session metadata and summaries
- `message_summaries_fts`: FTS5 full-text search index
- `index_queue`: Processing queue (internal use)

**Key Fields:**
- `message_uuid`: Unique message identifier
- `parent_uuid`: Parent message (tree structure)
- `session_id`: Conversation session
- `summary`: Smart-extracted searchable content
- `full_content`: Original message content
- `summary_method`: 'smart_extraction', 'too_short', or 'tool_noise'

---

## How Smart Extraction Works

1. **User Messages**: Full content indexed (avg 3.5K chars, important info upfront)
2. **Assistant Messages**: First 500 + last 200 chars + tool usage metadata
3. **Tool Noise**: Pure tool markers filtered automatically
4. **Short Messages**: Raw content used (< 50 chars)
5. **Instant**: No AI API calls, deterministic, ~1000+ messages/second

**Advantages:**
- Zero cost (no API calls)
- 100% coverage (never miss content)
- Instant indexing (no network latency)
- Deterministic (same input = same output)

---

## JSON Output Format

All commands support `--json` for structured output.

**Search results:**
```json
[
  {
    "message_uuid": "abc-123",
    "timestamp": "2025-01-13T10:30:00",
    "message_type": "user",
    "summary": "User asks about authentication bug",
    "project_path": "/home/user/projects/myapp",
    "conversation_summary": "Auth Bug Fix",
    "session_id": "session-xyz",
    "source": "claude_code",
    "depth": 3,
    "is_sidechain": false
  }
]
```

**Context results:**
```json
{
  "message": { /* target message */ },
  "parents": [ /* ancestor messages */ ],
  "children": [ /* responses */ ]
}
```

---

## Performance Tips

1. **Use `--days` to scope searches** - Faster and more relevant
2. **Start with summaries** - Only use `--content` when needed
3. **JIT indexing** - Skill runs `index --days 7` before search (instant)
4. **Periodic full reindex** - `ai-conversation-search index --all` monthly
5. **Project filtering** - Use `--project` for focused searches

---

## Supported Conversation Formats

### Claude Code (JSONL)
```jsonl
{"type": "summary", "leafUuid": "...", "conversationSummary": "..."}
{"uuid": "msg-1", "type": "user", "message": {...}, "timestamp": "..."}
{"uuid": "msg-2", "type": "assistant", "message": {...}, "parentUuid": "msg-1"}
```

### OpenCode (SQLite)
Reads directly from OpenCode's `opencode.db` database. Path can be overridden with `OPENCODE_HOME` env var.

### Codex CLI (JSONL)
Reads session files from `~/.codex/sessions/{year}/{month}/{day}/*.jsonl`.

**Key Features:**
- Multi-source unified search across all AI coding tools
- Preserves tree structure (branches, checkpoints) for Claude Code
- Filters tool noise automatically
- Handles multi-project setups
- Concurrent-safe with SQLite WAL mode

---

## Troubleshooting

**Search returns no results:**
- Check if database exists: `ls ~/.conversation-search/index.db`
- Run JIT index: `ai-conversation-search index --days 30`
- Verify conversations exist in at least one source:
  - Claude Code: `ls ~/.claude/projects/`
  - OpenCode: `ls ~/.local/share/opencode/opencode.db`
  - Codex CLI: `ls ~/.codex/sessions/`

**Database locked errors:**
- Close other instances of ai-conversation-search
- Database uses WAL mode for concurrent access
- Check permissions: `ls -la ~/.conversation-search/`

**Indexing seems slow:**
- Smart extraction is instant (~1000+ msgs/sec)
- If slow, check disk I/O or file system latency
- Try: `ai-conversation-search index --all` to rebuild

---

## Advanced Usage

**Batch operations:**
```bash
# Export all conversations about "database"
ai-conversation-search search "database" --json > database_convs.json

# Reindex specific time range
for days in 7 14 30; do
    ai-conversation-search index --days $days
done
```
