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

## Automatic Indexing

Already on. The plugin ships a Claude Code Stop hook that triggers background indexing when
a session ends, so a conversation is searchable as soon as it finishes. Indexing runs in a
detached process and the hook is capped at a 5 second timeout, so it never blocks your
session.

`ai-conversation-search setup-hooks` exists for manual (non-plugin) installs. If you ran it
before v0.16.0, remove the `ai-conversation-search hook` entry from your `settings.json` —
`setup-hooks` only ever looks inside `settings.json`, so it cannot see the plugin's own hook
and will not warn you; left in place, both fire and two indexers run at once.

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

> **Note:** `cargo install` installs only the core Rust binary. Interactive
> features (`pick`, `setup-hooks`) require the plugin wrapper script. Use the
> plugin install method above for the full experience.

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
