---
name: conversation-search
description: Find and resume past AI coding conversations (Claude Code, OpenCode, Codex CLI) by searching topics or filtering by date. Returns session IDs and project paths for easy resumption. Use when user asks "find that conversation about X", "what did we discuss", "what did we work on yesterday", "summarize today's work", "show this week's conversations", "recent projects we accomplished", or wants to locate past work by topic, date, or time period (yesterday, today, last week, specific dates).
allowed-tools: Bash, TodoWrite
---

# Conversation Search

Find past conversations across Claude Code, OpenCode, and Codex CLI and get the commands to resume them.

## MANDATORY FIRST STEP - CREATE TODO CHECKLIST

**Before doing ANYTHING else, you MUST use the TodoWrite tool to create this exact checklist:**

```
- Ensure ai-conversation-search tool is installed and upgraded
- Classify query type (temporal/topic/hybrid)
- Execute Level 1: focused search with ai-conversation-search
- Execute Level 2: broader search if Level 1 fails
- Execute Level 3: manual exploration if Level 1 and 2 fail
- Present results to user
```

**CRITICAL CONSTRAINTS:**
- DO NOT use grep, find, cat, or any manual file operations on .jsonl files
- DO NOT skip the todo creation step
- DO NOT jump to Level 3 without attempting Levels 1 and 2
- ONLY use ai-conversation-search commands for all search operations

Mark each todo as `in_progress` when starting it, `completed` when done.

## Prerequisites

The `ai-conversation-search` CLI is automatically managed by the plugin wrapper.
On first use, it downloads the correct binary for your platform and caches it.

**First todo: Verify tool is available**

```bash
ai-conversation-search --version
```

If the command is not found, the plugin may not be properly installed.
Guide the user: reinstall the plugin or visit https://github.com/yoshi47/ai-conversation-search

**Do not proceed with search** until the version check succeeds.

## Query Type Classification

**Second todo: Classify the user's query**

Determine which type before executing search:

### Type 1: Temporal Queries
User asks about time periods WITHOUT specific topics:
- "What did we work on yesterday?"
- "Summarize this week"
- "Show today's conversations"

**Action:** Use `list` command with date filters

### Type 2: Topic Queries
User asks about CONTENT/TOPICS:
- "Find that Redis conversation"
- "Where did we discuss authentication?"
- "Show me where we worked on the API"

**Action:** Use `search "topic"` command

### Type 3: Hybrid Queries
User asks about TOPIC + TIME:
- "Show me yesterday's authentication work"
- "Find Redis discussions from last week"
- "How many times did you say X in the past week?"

**Action:** Use `search "topic"` with date filters

## Three-Level Search Workflow

**Execute in order. Do not skip levels.**

### Level 1: Focused Search (ALWAYS START HERE)

Based on query classification:

**For Topic or Hybrid queries:**
```bash
ai-conversation-search search "search terms" --days 14 --json
```

**For Temporal queries:**
```bash
ai-conversation-search list --date yesterday --json  # or --days N, --since, --until
```

**Parse the JSON output.** If you find relevant matches → skip to Level 4 (present results).

**Note:** If results seem stale or incomplete, run `ai-conversation-search index` to update the index.

### Level 2: Broader Search

**Only if Level 1 found nothing useful.**

For topic/hybrid queries:
- Remove time constraints: `ai-conversation-search search "terms" --json`
- Try alternative keywords: "auth" vs "authentication"
- Try broader terms: "database" vs "postgres"

For temporal queries:
- Expand time range: `--days 30` instead of `--days 7`

**If matches found** → skip to Level 4.

### Level 3: Manual Exploration

**Only if Levels 1 and 2 both failed.**

1. List conversations: `ai-conversation-search list --days 30 --json`
2. Review conversation summaries in JSON
3. For promising sessions: `ai-conversation-search tree <SESSION_ID> --json`
4. Read message summaries to locate content

### Level 4: Present Results

**Format results for the user:**

For found conversations (results include a `source` field: `claude_code`, `opencode`, or `codex`):

```markdown
**Session Details**
- **Source**: Claude Code / OpenCode / Codex CLI
- **Session**: abc-123-session-id
- **Project**: /home/user/projects/myproject
- **Time**: 2025-11-13 22:50
- **Message**: def-456-message-uuid (if applicable)

**To Resume This Conversation**
```bash
cd /home/user/projects/myproject
claude --resume abc-123-session-id
```
```

Note: OpenCode sessions have `oc:` prefix, Codex sessions have `codex:` prefix in session IDs. For these sources, resume commands are tool-specific (not `claude --resume`).

For counting/analysis queries:
- Parse JSON results
- Filter by message_type if needed (user vs assistant)
- Count matches
- Present clear answer with evidence

**If not found after all 3 levels:**
- "No matching conversations found after exhaustive search"
- Suggest: `ai-conversation-search index --days 90` to reindex older history
- "The conversation may not exist or may be older than indexed range"

## Command Reference

### Search (for topic and hybrid queries)
```bash
# With time scope
ai-conversation-search search "query" --days N --json

# Specific date
ai-conversation-search search "query" --date yesterday --json
ai-conversation-search search "query" --date 2025-11-13 --json

# Date range
ai-conversation-search search "query" --since 2025-11-10 --until 2025-11-13 --json

# All time
ai-conversation-search search "query" --json

# Exact phrase match (prevents FTS5 operator injection)
ai-conversation-search search "query" --exact --json

# Group results by session (best match per session)
ai-conversation-search search "query" --group-by-session --json

# Show search diagnostics (session/message counts)
ai-conversation-search search "query" -v --json

# Filter by repository (partial match on repo root path)
ai-conversation-search search "query" --repo myproject --json

# Filter by source (claude_code, opencode, codex)
ai-conversation-search search "query" --source opencode --json
```

**Date filter options:**
- `--days N`: Last N days from now
- `--date DATE`: Specific calendar day
- `--since DATE`: From date onwards
- `--until DATE`: Up to date (inclusive)
- DATE formats: `yyyy-mm-dd`, `yesterday`, `today`
- Cannot mix `--days` with `--date/--since/--until`

**Other filter options:**
- `--repo REPO`: Filter by git repository root (partial match). Matches conversations from the same repo including worktrees and subdirectories.

### List (for temporal queries)
```bash
ai-conversation-search list --date yesterday --json
ai-conversation-search list --days 7 --json
ai-conversation-search list --since 2025-11-10 --until today --json

# Filter by repository
ai-conversation-search list --days 7 --repo myproject --json

# Filter by source
ai-conversation-search list --source codex --json
```

### Status
```bash
# Check index health, coverage, and unindexed files
ai-conversation-search status --json
```

### Context & Tree
```bash
ai-conversation-search context <UUID> --json
ai-conversation-search tree <SESSION_ID> --json
```

**Always use `--json` for structured output.**

### Interactive Session Picker (requires fzf + jq)
```bash
# Browse recent sessions interactively with fzf preview
ai-conversation-search pick

# Search and pick interactively
ai-conversation-search pick "authentication bug"

# Filter by project or time range
ai-conversation-search pick --days 30 --repo myproject

# Pick a session and execute the resume command
eval "$(ai-conversation-search pick)"
```

## Examples

**Example 1: Topic query**
```
User: "Find that conversation where we fixed the authentication bug"
```

Todo workflow:
1. ✓ Tool installed/upgraded
2. ✓ Classify: TOPIC query
3. ✓ Level 1: `ai-conversation-search search "authentication bug" --days 14 --json`
4. If no results → Level 2: `ai-conversation-search search "auth bug" --json`
5. Present results with resume commands

**Example 2: Temporal query**
```
User: "What did we work on yesterday?"
```

Todo workflow:
1. ✓ Tool installed/upgraded
2. ✓ Classify: TEMPORAL query
3. ✓ Level 1: `ai-conversation-search list --date yesterday --json`
4. Parse conversations, group by project
5. Present organized summary

**Example 3: Hybrid query**
```
User: "Show me yesterday's authentication work"
```

Todo workflow:
1. ✓ Tool installed/upgraded
2. ✓ Classify: HYBRID query (topic + time)
3. ✓ Level 1: `ai-conversation-search search "authentication" --date yesterday --json`
4. Present matching sessions

**Example 4: Counting/analysis query**
```
User: "How many times did you say 'absolutely right' in the past week?"
```

Todo workflow:
1. ✓ Tool installed/upgraded
2. ✓ Classify: HYBRID query (phrase + time)
3. ✓ Level 1: `ai-conversation-search search "absolutely right" --days 7 --json`
4. Parse JSON, filter `message_type == "assistant"`, count results
5. Present count with context snippets

## Error Handling

**Tool not installed:**
- Guide user through installation (see Prerequisites section)
- Do not proceed until confirmed

**Database not found:**
- User must run: `ai-conversation-search init`
- Creates `~/.conversation-search/index.db`

**Empty results:**
- Follow Level 1 → 2 → 3 progression
- Do not give up after Level 1
- Only report "not found" after Level 3 fails
