---
name: conversation-search
description: Find, review, and resume past AI coding conversations (Claude Code, OpenCode, Codex CLI) from their raw transcripts — returns the real session content, resumable session IDs, project paths, and the exact `claude --resume` command. Use whenever the user references a past session or prior conversation: to identify WHICH session something happened in, resume it, locate where a topic/PR/issue was discussed, or understand WHAT was discussed or decided earlier. The word "session"/"セッション" or any reference to a past conversation is the trigger — even when the user only wants the content, read the raw transcripts. A raw session UUID in the prompt ALWAYS triggers this skill — even when the transcript path looks obvious, and even when the session is only input to another task (verifying its conclusion, continuing its work); never find/grep ~/.claude/projects manually. Triggers: "find that conversation about X", "which session was that", "what did we discuss/decide", "resume that conversation", a session UUID, a GitHub PR/issue URL about past work. Japanese: "どのセッション", "過去のセッション(を確認/把握)して", "このセッションで調査してた <uuid>", "(そのセッションの)結論は合ってる/的を得てる？", "どこで話した/やった/確認した", "確認してた", "どんな内容/話だった(っけ)", "中身を把握", "～だっけ？", "あの会話", "あのPR/issue", "resume/続きやりたい".
allowed-tools: Bash, TodoWrite
---

# Conversation Search

Find past conversations across Claude Code, OpenCode, and Codex CLI and get the commands to resume them.

## MANDATORY FIRST STEP - CREATE TODO CHECKLIST

**Before doing ANYTHING else, you MUST use the TodoWrite tool to create this exact checklist:**

```
- Ensure ai-conversation-search tool is installed and upgraded
- Check for a session ID in the prompt (Level 0: read it directly, skip the rest)
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
- ONLY use ai-conversation-search commands to *locate and read* conversations
- Piping this tool's own `--json` output through `jq` is fine and expected — but reach for
  the built-in flags first (`--role`, `--no-tools`, `--flat`, `--content-chars`), which
  cover the common filters and return far less text

Mark each todo as `in_progress` when starting it, `completed` when done.

## When to Use This Skill

This skill reads the **raw `.jsonl` transcripts** of past sessions and returns
**resumable session IDs**, project paths, and the exact `claude --resume`
command — the real conversation, not a reconstruction.

**Decision rule:** If the user **mentions a session or a past/prior conversation
in ANY form** — including just wanting to *understand its content* — use this
skill. Reading the actual transcript is the right move even when the user only
wants to know "what was discussed", because the transcript is the source of truth.

| User intent | Use this skill |
|---|---|
| "Which session was that?" / "どのセッション？" | ✅ |
| "Resume that conversation" / "続きやりたい" | ✅ |
| "Check past sessions and tell me what it was about" / "過去のセッションを確認して把握して" | ✅ |
| "What was discussed / decided in that session?" / "どんな内容/話だったっけ？" | ✅ |
| User pastes a GitHub PR/issue URL and asks where it was discussed | ✅ |
| Find a session by raw text (PR number, error message, exact phrase) | ✅ |
| User gives a session UUID and asks to verify/continue that work / "このセッションで調査してた <uuid>、結論合ってる？" | ✅ |

A session UUID in the prompt is decisive **even when it makes searching look
unnecessary**: do not `find`/`grep` `~/.claude/projects` manually — read it with
`ai-conversation-search tree <SESSION_ID>` (then `context <MESSAGE_UUID>` to
expand around a specific message).

The presence of the word **"session" / "セッション"** or a reference to a past
conversation is decisive — use this skill regardless of whether the user
explicitly says "resume".

## Prerequisites

The `ai-conversation-search` CLI is automatically managed by the plugin wrapper.
On first use, it downloads the correct binary for your platform and caches it.

**First todo: Verify tool is available**

```bash
ai-conversation-search --version
```

If the command is not found, the plugin may not be properly installed.
Guide the user: reinstall the plugin or visit https://github.com/yoshi47/ai-conversation-search

**The reported version must be 0.16.0 or newer.** Two things break below it. Before 0.15.0,
`--json` returns a bare array instead of the `{"results": [...]}` envelope this skill
assumes, so `.results[]` yields nothing and every search looks like "no matches" — a wrong
answer, not an error. Before 0.16.0, the `tree` flags used throughout this skill
(`--role`, `--no-tools`, `--flat`, `--content`) are rejected, and `tree` will not
auto-index a session that just ended. Stop and tell the user to upgrade rather than
reporting an empty result.

Do not rely on a command being *rejected* to notice a stale binary: the envelope change
rejects nothing. Check `command -v ai-conversation-search` — a manually installed binary
(e.g. in `~/.local/bin` or `~/.cargo/bin`) can shadow the plugin wrapper and will not
auto-upgrade; refresh that binary or adjust PATH before trusting search results.

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

## Search Workflow

**Check Level 0 first. If it does not apply, execute Levels 1-3 in order without skipping.**

### Level 0: A Session ID Was Given (SKIP THE SEARCH ENTIRELY)

If the prompt contains a session UUID — or any prefix of one of 8+ characters — you already
know which conversation to read. Searching for it is pointless; go straight to the
transcript:

```bash
ai-conversation-search tree <SESSION_ID> --json --role user --no-tools --flat --content --content-chars 500
```

That returns just the human's messages, in order, with bodies. Drop `--role user` to see
the assistant's replies too, and raise `--content-chars` when you need full text.

**A session that just ended is fine.** `tree` indexes the transcript on the spot when the
id is not in the index yet, so there is no need to run `index` first — and no reason to
conclude the session does not exist from a single `not found`.

Go to Level 4 with what you find.

### Level 1: Focused Search (START HERE WHEN NO SESSION ID WAS GIVEN)

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
3. For promising sessions: `ai-conversation-search tree <SESSION_ID> --json --no-tools --flat`
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

**If not found after Levels 1-3:**
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

# Raise the result cap (default 20 — see below)
ai-conversation-search search "query" --limit 50 --json
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
- `--limit N`: Max results (**default: 20**). Results are capped at this value. When the cap drops matches, a `Note: showing first N results (more matches exist)` line is printed to stderr — do not read a capped list as "nothing else exists". Raise it before concluding a topic is absent.
- `--content`: Show fuller message content instead of the 200-character snippet. Works for
  human output, `--group-by-session`, and `--json` (which gains `full_content` and
  `full_content_truncated` per row). Capped at `--content-chars` (default 300) in both
  modes — raise it deliberately, since 50 uncapped bodies run to ~175KB of context.

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
ai-conversation-search context <MESSAGE_UUID> --json
ai-conversation-search tree <SESSION_ID> --json

# SESSION_ID accepts a full UUID or any unique prefix
ai-conversation-search tree 1c538017 --json
```

`tree` reports an error instead of guessing when a prefix matches more than one
session; pass more characters to disambiguate. When the id resolves to nothing, `tree`
indexes that session's transcript and retries once, so a conversation that ended moments
ago is readable without running `index`.

**`tree` options** — use these instead of post-processing the tree yourself:

| Flag | Effect |
|---|---|
| `--role user` / `--role assistant` | Keep only that side of the conversation |
| `--no-tools` | Drop `[Tool: X]` / `[Tool result]` / interrupt nodes, and empty bodies |
| `--flat` | Return a flat list instead of nested `children`, in timestamp order |
| `--content` | Include message bodies (omitted by default) |
| `--content-chars N` | Cap each body at N characters (default 300) |

Read what the human actually said in one command:

```bash
ai-conversation-search tree <SESSION_ID> --json --role user --no-tools --flat --content --content-chars 500
```

Filtering keeps `total_messages` at the session total and adds `returned_messages` for the
count you got back. If a filter matches nothing, `.warning` says so — that is not the same
as an empty conversation.

Bodies are opt-in on purpose: a long session serialises to hundreds of KB, which is worth
avoiding unless you need the text. Nodes carry `full_content_truncated` so you can tell a
capped body from a complete one.

Agent completion notices appear as user messages prefixed `[Task notification] `. They are
often worth reading — that is where a subagent's findings live — but the prefix lets you
skip them when you only want what the human typed.

**Always use `--json` for structured output.**

`search`, `search --group-by-session` and `list` return `{"results": [...], "truncated": bool}` —
read rows from `.results`, and check `.truncated`. When it is `true`, `--limit` cut the answer
off and "no match" is not a conclusion you can draw yet; re-run with a higher `--limit`.
`tree`, `context` and `status` return their own objects, not this envelope.

### Interactive Session Picker (requires fzf 0.28+ and jq)
Typing in the picker runs a **live full-text search against message bodies**
via SQLite FTS5 (trigram tokenizer, Japanese/CJK friendly). Matching happens
server-side on every keystroke, so the picker is not limited to titles/summaries.

```bash
# Browse recent sessions and filter by typing (searches titles + body)
ai-conversation-search pick

# Pre-fill the query at startup
ai-conversation-search pick "authentication bug"

# Scope to current directory (90-day window, up to 100 sessions)
ai-conversation-search pick --here

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

**Example 5: GitHub PR/Issue URL**
```
User: "このPRのレビューしてたのってどのセッション？ https://github.com/org/repo/pull/23064"
```

PR/Issue numbers appear verbatim in transcripts → high-precision FTS hit.

Todo workflow:
1. ✓ Tool installed/upgraded
2. ✓ Classify: TOPIC query (number is the search term)
3. Extract the PR number from the URL (e.g. `23064`)
4. ✓ Level 1: `ai-conversation-search search "23064" --json`
5. Present matching sessions with `claude --resume` commands

Stay with `ai-conversation-search` for this — manual `grep` over `.jsonl` files
returns non-resumable results and misses the structured session metadata.

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
