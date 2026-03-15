# Installation Instructions

Thank you for installing the **conversation-search** plugin!

## Step 1: Install the CLI Tool

The skill requires the `ai-conversation-search` CLI tool.

### Download pre-built binary

```bash
# Detect platform and download
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

# Ensure ~/.local/bin is in your PATH
export PATH="$HOME/.local/bin:$PATH"
```

### Alternative: Build from source

```bash
cargo install --git https://github.com/yoshi47/ai-conversation-search
```

## Step 2: Initialize the Database

Create the search index for your conversation history:

```bash
ai-conversation-search init
```

This will:
- Create `~/.conversation-search/index.db`
- Index your last 7 days of conversations
- Extract searchable content using smart hybrid extraction (instant, no AI calls)

## Step 3: Test the Installation

Verify everything is working:

```bash
ai-conversation-search search "test" --json
```

## You're Ready!

The **conversation-search** skill is now active. Try asking Claude:

- "Find that message where we discussed authentication"
- "What did we talk about regarding React hooks?"
- "Locate the conversation where we fixed the database bug"

Claude will use a progressive search strategy to find specific message UUIDs you can branch from.

## Troubleshooting

**Tool not found:**
- Make sure `ai-conversation-search` is in your PATH
- Try: `which ai-conversation-search`

**No conversations found:**
- Verify `~/.claude/projects/` exists and contains .jsonl files
- Try: `ai-conversation-search list --days 30`

**For help:**
- Documentation: https://github.com/yoshi47/ai-conversation-search
- Issues: https://github.com/yoshi47/ai-conversation-search/issues
