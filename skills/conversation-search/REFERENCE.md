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
ai-conversation-search search QUERY [--exact] [--days DAYS] [--since DATE] [--until DATE] [--date DATE] [--project PROJECT] [--repo REPO] [--source SOURCE] [--limit LIMIT] [--sort SORT] [--content] [--content-chars N] [--group-by-session] [-v] [--json]
```

**Arguments:**
- `QUERY`: Search query (supports FTS5 syntax)

**Options:**
- `--exact`: Exact phrase match (wraps query in quotes, prevents FTS5 operator injection)
- `--days DAYS`: Limit to last N days
- `--since DATE`: Start date (YYYY-MM-DD, yesterday, today)
- `--until DATE`: End date (YYYY-MM-DD, yesterday, today)
- `--date DATE`: Specific date (YYYY-MM-DD, yesterday, today)
- `--project PROJECT`: Filter by project path
- `--repo REPO`: Filter by repository root (partial match)
- `--source SOURCE`: Filter by source (`claude_code`, `opencode`, `codex`)
- `--limit LIMIT`: Max results (default: 20). When the cap drops matches, `Note: showing first N results (more matches exist)` is printed to stderr regardless of `-v`
- `--sort SORT`: Result order — `relevance` (bm25, default) or `recent` (newest first)
- `--content`: Show message bodies instead of snippets. Applies to human output,
  `--group-by-session`, and `--json`; in JSON each row gains `full_content` and
  `full_content_truncated`
- `--content-chars`: Max characters of body to show with `--content` (default: 300).
  Applies to JSON as well as human output — an uncapped body averages 3.5K characters, so
  `--limit 50 --content` would be ~175KB
- `--group-by-session`: Group results by session (show the top-ranked match per session with match count)
- `-v, --verbose`: Show search diagnostics (sessions scanned, messages matched, unindexed warnings)
- `--json`: Output as JSON (includes `resume_command` field for Claude Code sessions).
  `resume_command` is shell-quoted and safe to `eval`. It is `null` for OpenCode/Codex
  sessions, and also `null` when the project path or session id cannot be expressed safely
  in a shell command — treat `null` as "resume manually", not as an error.

**Search Syntax:**
- Simple: `authentication bug`
- Multiple terms: `react hooks useEffect` (implicit OR, ranked by relevance — documents matching more terms rank higher)
- Phrases: `"exact phrase"` (or use `--exact`)
- Operators: `auth AND bug`, `react OR vue`

**Ranking:** Results are ordered by bm25 relevance by default. Because bm25 normalizes by
document length, very long machine-generated transcripts sink automatically. Use
`--sort=recent` when you want to browse chronologically instead.

**Short terms (under 3 characters):** the trigram tokenizer needs 3+ characters, so short
terms cannot be ranked by. They are applied as a **mandatory substring filter** on top of
the FTS results instead — a result must contain every short term, while the longer terms
are OR-joined and ranked.

Example: in `パッケージ 更新 ドキュメント`, `更新` is 2 characters, so results are ranked
by `パッケージ`/`ドキュメント` relevance but must all contain `更新`.

**Only if _every_ term is under 3 characters** (e.g. `認証 実装`) does the whole query fall
back to substring matching: AND semantics, recency order, and `--sort` has no effect. An
empty query behaves the same way.

Escape hatch: quoting any part of the query — `"..."` or `--exact` — sends it to FTS
verbatim.

Explicit `AND`/`OR`/`NOT` operators also go to FTS verbatim, but **only if every operand is
3+ characters**. Otherwise the query takes the substring-matching path and the operator is
treated as a literal word — a sub-3-character operand cannot be expressed in FTS at all, so
honoring the operator would silently drop it. Quote the short operand to force FTS.

Case sensitivity differs by term length: long terms fold case across Unicode, short terms
only across ASCII (they go through `LIKE`).

**Note:** Cannot mix `--days` with `--date/--since/--until`.

**Examples:**
```bash
# Basic search
ai-conversation-search search "authentication"

# Exact phrase match (safe from FTS5 injection)
ai-conversation-search search "exact phrase here" --exact

# Time-scoped search
ai-conversation-search search "database" --days 30

# Calendar date filtering
ai-conversation-search search "auth" --date yesterday --json
ai-conversation-search search "hooks" --since 2025-11-10 --until 2025-11-13

# Group by session
ai-conversation-search search "api" --group-by-session --json

# Project-specific search
ai-conversation-search search "api" --project /home/user/myapp

# Get JSON output (for programmatic use)
ai-conversation-search search "hooks" --json
```

---

### ai-conversation-search status

Show index health, coverage, and statistics.

```bash
ai-conversation-search status [--json]
```

**Options:**
- `--json`: Output as JSON

**What it shows:**
- Database location and size
- FTS5 health check (OK / CORRUPTED)
- Total sessions and messages, broken down by source
- Date coverage (earliest to latest conversation)
- Top repositories by session count
- Indexed files vs files on disk (with warnings for unindexed files)

**Example:**
```bash
# Human-readable status
ai-conversation-search status

# JSON output (for programmatic use)
ai-conversation-search status --json
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
ai-conversation-search list [--days DAYS] [--since DATE] [--until DATE] [--date DATE] [--limit LIMIT] [--repo REPO] [--source SOURCE] [--json]
```

**Options:**
- `--days DAYS`: Show conversations from last N days (default: 7)
- `--since DATE`: Start date (YYYY-MM-DD, yesterday, today)
- `--until DATE`: End date (YYYY-MM-DD, yesterday, today)
- `--date DATE`: Specific date (YYYY-MM-DD, yesterday, today)
- `--limit LIMIT`: Max conversations to show (default: 20)
- `--repo REPO`: Filter by repository root (partial match)
- `--source SOURCE`: Filter by source (`claude_code`, `opencode`, `codex`)
- `--json`: Output as JSON (includes `resume_command` field for Claude Code sessions).
  `resume_command` is shell-quoted and safe to `eval`. It is `null` for OpenCode/Codex
  sessions, and also `null` when the project path or session id cannot be expressed safely
  in a shell command — treat `null` as "resume manually", not as an error.

**Note:** Cannot mix `--days` with `--date/--since/--until`.

**Example:**
```bash
# List last week's conversations
ai-conversation-search list --days 7

# List yesterday's conversations
ai-conversation-search list --date yesterday --json

# List by date range
ai-conversation-search list --since 2025-11-10 --until today --json

# List last 50 conversations
ai-conversation-search list --limit 50 --json
```

---

### ai-conversation-search tree

Show the conversation tree structure for a session.

```bash
ai-conversation-search tree SESSION_ID [--role ROLE] [--no-tools] [--flat] [--content] [--content-chars N] [--json]
```

**Arguments:**
- `SESSION_ID`: Session ID from list or search results. A unique prefix is accepted; an ambiguous prefix is reported as an error with the number of matches rather than resolved to one of them. Bare UUIDs also resolve to `oc:`/`codex:`-prefixed OpenCode and Codex sessions.

**Options:**
- `--role user|assistant`: Keep only messages from that side
- `--no-tools`: Drop tool-call, tool-result and interrupt nodes, plus any message whose body extracted to nothing (thinking-only assistant turns)
- `--flat`: Return a flat list instead of nested `children`, ordered by timestamp. Sessions with more than one tree root (resume, sidechain, pruned parent) interleave in time, so the nested order is not chronological — take the last N entries of a `--flat` result when you want the most recent messages
- `--content`: Include message bodies (omitted by default)
- `--content-chars N`: Cap each body at N characters (default: 300, requires `--content`)
- `--json`: Output as JSON

**Automatic indexing:** when `SESSION_ID` resolves to nothing, `tree` indexes that
session's transcript and retries once, so a conversation that just ended is readable
without running `index` first. This covers Claude Code sessions only — OpenCode and Codex
ids are not stored in that layout — and it will not resurrect a claude-mem observer
session, which stays excluded (see `index`).

**Filtering and counts:** `total_messages` always means "messages in the session".
`returned_messages` is how many survived the filters. When a filter matches nothing,
`.warning` says so and the exit status stays `0` — filtered-to-empty is not the same as an
empty conversation, and the exit status alone cannot tell you which you have.

**Message bodies are opt-in** (since 0.16.0). Without `--content` no `full_content` is
emitted at all: a long session serialises to hundreds of KB, which is worth avoiding unless
the text is actually needed. With `--content`, every node carries
`full_content_truncated` so a capped body is distinguishable from a complete one.

`depth` is the message's depth in the original transcript. With `--role` or `--no-tools`,
surviving nodes are lifted into a filtered parent's place and their `parent_uuid` is
repointed at the nearest surviving ancestor, so `depth` may exceed the nesting you see.

**Agent notifications:** subagent completion notices are indexed as user messages prefixed
`[Task notification] `. They are tagged rather than suppressed because the body carries the
agent's actual result.

Messages indexed before 0.16.0 keep the untagged form permanently — re-indexing does not
backfill them. `--force` only bypasses the file-level mtime skip; for a session already in
the index, only messages whose UUID is not yet stored get inserted, so a change to how
content is derived never reaches existing rows. Backfill with SQL if you need it:

```sql
UPDATE messages SET full_content = '[Task notification] ' || full_content
WHERE message_type = 'user' AND full_content LIKE '<task-notification>%';
```

**Exit status:** `1` when no tree came back — the session id could not be resolved (not
found, or an ambiguous prefix), *or* it resolved but its transcript could not be read. The
reason is in `.error` in JSON mode and on stderr otherwise; read it before deciding which
of those happened. `0` when a tree came back, including when `.warning` is set — a warning
means partial data was returned and is worth reading, not discarding. Do not read a
non-zero exit as "the conversation is empty".

**Use case:** Visualize conversation branching and checkpoint structure.

Each node's `summary` is the first non-empty line of the message (120 characters).
Human output truncates it further to 80 characters; `--json` carries the full value.

**Example:**
```bash
ai-conversation-search tree session-abc-123
ai-conversation-search tree session-     # unique prefix
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
- `--force`: Re-read files even if unchanged since the last run. `--all` only widens the date window, so files already recorded as processed need this to be revisited
- `--no-extract`: Skip smart extraction

**Not indexed:** claude-mem observer sessions. They mirror another session's tool calls
and carry claude-mem's generated observations, both of which are stored elsewhere — the
primary session and claude-mem's own database. Set
`CONVERSATION_SEARCH_INDEX_OBSERVER=1` together with `--all --force` to index them anyway.

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

### ai-conversation-search prune-observer

Remove claude-mem observer sessions that earlier versions put in the index.

```bash
ai-conversation-search prune-observer [--dry-run] [--yes]
```

**Options:**
- `--dry-run`: Report how many sessions would be removed, without changing anything
- `--yes`: Skip the confirmation prompt. Required when stdin is not a terminal

**Agent usage:** an agent shell never has a terminal, so the plain command exits 1 with
`prune-observer requires --yes when stdin is not a terminal`. Run `--dry-run`, show the
count to the user, and let them decide — do not reach for `--yes` on your own. This deletes
rows and cannot be undone.

**Notes:**
- Irreversible, and can take several minutes on a large index (it rebuilds the FTS index
  to clear entries stranded by the pre-0.15.0 delete trigger). Back up
  `~/.conversation-search/index.db` first.
- The database file does not shrink; freed pages are reused by later indexing.
- Nothing is lost: the observations live in `~/.claude-mem/claude-mem.db`, and the mirrored
  tool calls live in the primary sessions, which stay indexed.
- New observer sessions are skipped at index time, so this only needs running once.

---

### ai-conversation-search hook

Trigger background indexing if the TTL-based debounce has expired. Designed to be called from a Claude Code Stop hook.

```bash
ai-conversation-search hook
```

**Behavior:**
- Checks stamp file TTL (default: 60s, configurable via `CONVERSATION_SEARCH_HOOK_TTL`).
  This is the hook's own TTL — `CONVERSATION_SEARCH_INDEX_TTL` (300s) governs the
  auto-index that `search`, `tree` and `list` trigger, and does not apply here. The two are
  separate because all of those commands touch the same stamp, so at 300s a session in
  which the agent searched would leave the hook a no-op
- If fresh: exits immediately (< 1ms, two stat() calls)
- If stale: spawns background `index --days 1` and exits immediately
- Always exits 0 — never fails or blocks the caller

---

### ai-conversation-search setup-hooks

Add a Claude Code Stop hook that triggers automatic background indexing.

```bash
ai-conversation-search setup-hooks [--settings-file PATH]
```

**Options:**
- `--settings-file PATH`: Override settings.json path (default: `~/.claude/settings.json`)

**What it does:**
- Adds a Stop hook entry to Claude Code settings.json
- Idempotent: safe to run multiple times
- Uses atomic write (mktemp + mv) to prevent corruption

**Example:**
```bash
ai-conversation-search setup-hooks
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

**Two shapes.** `search`, `search --group-by-session` and `list` return an *envelope*:
rows live under `.results`, and `.truncated` says whether `--limit` cut the answer off.
Always read `.truncated`. `true` means `--limit` may have cut the answer off — re-run with
a higher `--limit` before concluding something is not there. On `search` it errs toward
`true`, so a `true` is worth re-checking rather than trusting as proof more exists.
`tree`, `context` and `status` return their own objects, unchanged.

```
search / search --group-by-session / list  →  { "results": [ … ], "truncated": false }
tree / context / status                    →  a command-specific object
```

**Search results:**
```json
{
  "results": [
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
      "is_sidechain": false,
      "resume_command": "cd -- /home/user/projects/myapp && claude --resume session-xyz"
    }
  ],
  "truncated": false
}
```

**Search results with `--group-by-session`:**
```json
{
  "results": [
    {
      "message_uuid": "abc-123",
      "session_id": "session-xyz",
      "source": "claude_code",
      "match_count": 5,
      "resume_command": "cd -- /home/user/projects/myapp && claude --resume session-xyz"
    }
  ],
  "truncated": false
}
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
