# Installation Instructions

Thank you for installing the **conversation-search** plugin!

## Automatic Setup

The `ai-conversation-search` CLI tool is automatically downloaded and managed by the plugin.
On first use, it will:
- Download the correct binary for your platform (macOS/Linux, arm64/x86_64)
- Initialize the search index with your last 7 days of conversations

## Test the Installation

Verify everything is working:

```bash
ai-conversation-search search "test" --json
```

## You're Ready!

The **conversation-search** skill is now active. Try asking Claude:

- "Find that message where we discussed authentication"
- "What did we talk about regarding React hooks?"
- "Locate the conversation where we fixed the database bug"

## Alternative: Build from Source

If you prefer to build from source instead of using the auto-managed binary:

```bash
cargo install --git https://github.com/yoshi47/ai-conversation-search
ai-conversation-search init
```

## Troubleshooting

**Tool not found:**
- The plugin wrapper should make the command available automatically
- Try: `which ai-conversation-search`
- Reinstall the plugin if needed

**No conversations found:**
- Verify `~/.claude/projects/` exists and contains .jsonl files
- Try: `ai-conversation-search list --days 30`

**For help:**
- Documentation: https://github.com/yoshi47/ai-conversation-search
- Issues: https://github.com/yoshi47/ai-conversation-search/issues
