# Conversation Search

Find and resume past AI coding conversations using smart hybrid extraction and JIT indexing. Supports **Claude Code**, **OpenCode**, and **Codex CLI** — search across all your AI conversations from a single tool.

## Features

- **Multi-Source Support**: Search across Claude Code, OpenCode, and Codex CLI conversations
- **Session Resumption**: Get exact commands to resume past conversations
- **Unified CLI**: Single `ai-conversation-search` command with intuitive subcommands
- **Calendar Date Filtering**: Intuitive `--date yesterday`, `--since`, `--until` parameters
- **Smart Extraction**: Hybrid indexing (full user content + smart assistant extraction)
- **Background Auto-Indexing**: Automatic background indexing with TTL-based cooldown (no CLI blocking)
- **Local Timezone Display**: All timestamps shown in your local time
- **Meta-Conversation Filtering**: Automatically excludes search tool usage from results
- **Progressive Exploration**: Simple search → broader search → manual exploration
- **Conversation Context**: Expand context incrementally around any message
- **Claude Code Skill**: Integrated Skill that outputs session resumption commands
- **Multi-Project Support**: Works across all your AI coding projects

## Quick Start

### Installation via Claude Code Plugin (Recommended)

Install the complete plugin (skill + CLI tool instructions) directly in Claude Code:

```bash
# Add this repo's marketplace
/plugin marketplace add yoshi47/ai-conversation-search

# Install the plugin
/plugin install conversation-search
```

Then follow the installation instructions shown by Claude to install the CLI tool and initialize the database.

### Manual Installation

#### 1. Install CLI Tool

```bash
# Download pre-built binary (macOS/Linux)
ARCH=$(uname -m)
OS=$(uname -s)
case "${OS}-${ARCH}" in
    Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
    Darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
    Linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
    Linux-aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
    *) echo "Unsupported platform: ${OS}-${ARCH}"; exit 1 ;;
esac
mkdir -p ~/.local/bin
curl -fsSL "https://github.com/yoshi47/ai-conversation-search/releases/latest/download/ai-conversation-search-${TARGET}" \
    -o ~/.local/bin/ai-conversation-search && chmod +x ~/.local/bin/ai-conversation-search

# Make sure ~/.local/bin is in your PATH
export PATH="$HOME/.local/bin:$PATH"
```

Or build from source:
```bash
cargo install --git https://github.com/yoshi47/ai-conversation-search
```

#### 2. Initialize Database

```bash
ai-conversation-search init
```

This creates the database and indexes your last 7 days of conversations from all supported sources (Claude Code, OpenCode, Codex CLI).

#### 3. Install Skill (Optional)

```bash
mkdir -p ~/.claude/skills/conversation-search
cp skills/conversation-search/* ~/.claude/skills/conversation-search/
```

### Basic Usage

```bash
# Search for conversations (shows session ID and resume commands)
ai-conversation-search search "authentication bug"

# Search with calendar date filters
ai-conversation-search search "react hooks" --date yesterday
ai-conversation-search search "auth" --since 2025-11-10 --until 2025-11-13

# List conversations by date
ai-conversation-search list --date yesterday
ai-conversation-search list --since "2025-11-01"

# Traditional relative time filters still work
ai-conversation-search search "query" --days 30

# Filter by source
ai-conversation-search search "query" --source opencode
ai-conversation-search list --source codex --days 7

# Get resume commands for a specific message
ai-conversation-search resume <MESSAGE_UUID>
```

### Using with Claude Code Skill

Once installed, ask Claude:

**Topic-based queries:**
- "Find that conversation where we discussed authentication"
- "Locate the conversation about React hooks"
- "What did we talk about regarding the database?"

**Temporal queries:**
- "What did we work on yesterday?"
- "Summarize today's conversations"
- "Show me this week's work"

**Hybrid queries:**
- "Find yesterday's authentication work"
- "Show recent Redis discussions"

**Auto-Installation & Upgrade**: If the CLI tool isn't installed, the skill will automatically install it, then initialize the database. If a newer version is available, it auto-upgrades. In most cases, everything "just works" after installing the plugin!

Claude will show you the session ID, project path, and exact commands to resume the conversation.

## Command Reference

### `ai-conversation-search init`
Initialize database and perform initial indexing
```bash
ai-conversation-search init [--days 7] [--no-extract] [--force]
```

### `ai-conversation-search index`
JIT index conversations (instant, no AI calls)
```bash
ai-conversation-search index [--days N] [--all] [--no-extract]
```

**Note**: Indexing runs automatically in the background. Run `index` manually if results seem stale.

### `ai-conversation-search search`
Search conversations with flexible date filtering
```bash
# Traditional relative time
ai-conversation-search search "query" [--days N] [--project PATH] [--source SOURCE] [--content] [--json]

# Calendar date filtering
ai-conversation-search search "query" --date yesterday [--json]
ai-conversation-search search "query" --date 2025-11-13 [--json]
ai-conversation-search search "query" --since 2025-11-10 --until 2025-11-13 [--json]

# Date formats: YYYY-MM-DD, "yesterday", "today"
# Note: --days cannot be combined with --date/--since/--until

# Filter by source (claude_code, opencode, codex)
ai-conversation-search search "query" --source opencode --json
```

### `ai-conversation-search context`
Get context around a specific message
```bash
ai-conversation-search context MESSAGE_UUID [--depth 5] [--content] [--json]
```

### `ai-conversation-search list`
List recent conversations with calendar date support
```bash
# Traditional relative time
ai-conversation-search list [--days 7] [--limit 20] [--source SOURCE] [--json]

# Calendar date filtering
ai-conversation-search list --date yesterday [--json]
ai-conversation-search list --since 2025-11-10 --until today [--json]
```

### `ai-conversation-search tree`
View conversation tree structure
```bash
ai-conversation-search tree SESSION_ID [--json]
```

## Supported Sources

| Source | Data Location | Session Prefix |
|--------|--------------|---------------|
| **Claude Code** | `~/.claude/projects/{project}/{session}.jsonl` | *(none)* |
| **OpenCode** | `~/.local/share/opencode/opencode.db` (SQLite) | `oc:` |
| **Codex CLI** | `~/.codex/sessions/{year}/{month}/{day}/*.jsonl` | `codex:` |

- OpenCode DB path can be overridden with `OPENCODE_HOME` environment variable
- All sources are automatically detected and indexed together
- Results are tagged with source labels: `[CC]` (Claude Code), `[OC]` (OpenCode), `[CX]` (Codex CLI)

## Architecture

```
~/.claude/
├── projects/           # Claude Code conversation files (JSONL)
│   └── {project}/
│       └── {session}.jsonl
└── skills/
    └── conversation-search/  # Optional Skill

~/.local/share/opencode/
└── opencode.db         # OpenCode conversations (SQLite)

~/.codex/sessions/
└── {year}/{month}/{day}/
    └── *.jsonl          # Codex CLI session files

~/.conversation-search/
└── index.db           # Unified search database (all sources)
```

**Key Purpose**: Find session IDs and project paths to resume past conversations across all AI coding tools.

## How It Works

1. **Multi-Source Indexer**: Scans Claude Code (JSONL), OpenCode (SQLite), and Codex CLI (JSONL) conversations
2. **Smart Extraction**: Hybrid approach - full user content + first 500/last 200 chars for assistant
3. **Meta-Conversation Filtering**: Automatically detects and excludes conversations where Claude used the search tool (prevents search results pollution)
4. **Search**: FTS5 full-text search over extracted content with conversation tree traversal
5. **Calendar Date Filtering**: Intuitive date parameters (`--date yesterday`) using SQLite date functions
6. **Background Auto-Indexing**: Automatic background indexing with TTL-based cooldown (never blocks CLI)
7. **Local Timezone Display**: All timestamps converted to your local timezone for readability

## Advanced Usage

### JSON Output for Scripting

All commands support `--json` flag:
```bash
# Export search results
ai-conversation-search search "authentication" --json > auth_convs.json

# Programmatic processing
ai-conversation-search list --days 30 --json | jq '.[] | .conversation_summary'
```

## Configuration

**Database location:** `~/.conversation-search/index.db`

**No configuration file needed** - all settings via command-line flags.

## Performance

- **Smart Extraction**: Instant (no AI calls), deterministic
- **Indexing Speed**: ~1000+ messages/second (no API latency)
- **Storage**: ~1-2KB per message (extracted text + metadata)
- **Search Speed**: SQLite FTS5 is very fast, even with 100K+ messages
- **Cost**: $0 (no AI API calls during indexing)

## Development

### Setup

```bash
git clone https://github.com/yoshi47/ai-conversation-search
cd ai-conversation-search
cargo build
```

### Run Tests

```bash
cargo test
```

### Project Structure

```
ai-conversation-search/
├── src/
│   ├── main.rs                # Entry point
│   ├── cli.rs                 # CLI argument parsing and commands
│   ├── search.rs              # Search functionality + date filtering
│   ├── summarization.rs       # Smart hybrid extraction
│   ├── date_utils.rs          # Calendar date parsing
│   ├── db.rs                  # Database connection
│   ├── schema.rs              # Schema migrations
│   ├── error.rs               # Error types
│   ├── git_utils.rs           # Git repo root detection
│   └── indexer/
│       ├── mod.rs             # Claude Code indexing + meta-filtering
│       ├── codex.rs           # Codex CLI conversation indexing
│       └── opencode.rs        # OpenCode conversation indexing
├── data/
│   └── schema.sql             # Database schema
├── skills/
│   └── conversation-search/
│       ├── SKILL.md           # Claude Code Skill with query classification
│       └── REFERENCE.md       # Complete command reference
├── Cargo.toml
└── README.md
```

## Troubleshooting

**"Database not found" error:**
```bash
ai-conversation-search init
```

**"No conversations found":**
- Verify conversation data exists in at least one source:
  - Claude Code: `~/.claude/projects/` contains JSONL files
  - OpenCode: `~/.local/share/opencode/opencode.db` exists
  - Codex CLI: `~/.codex/sessions/` contains JSONL files
- Use one of the supported AI coding tools to create some conversations first

**Skill not activating:**
- Check Skill location: `ls ~/.claude/skills/conversation-search/SKILL.md`
- Verify YAML frontmatter format
- Restart Claude Code
- Try explicit trigger: "Search my conversations for X"

## Contributing

PRs welcome! This is an experimental tool to improve Claude Code workflow.

### Areas for Contribution

- Vector embeddings for semantic similarity search
- Web UI for conversation tree visualization
- Export conversation branches as markdown
- Conversation analytics (topics, frequency, etc.)
- Additional Claude Code Skills using the search API

## License

MIT

## Acknowledgments

Built for the Claude Code ecosystem. Uses smart hybrid extraction for instant, cost-free indexing.
