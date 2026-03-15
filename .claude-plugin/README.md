# Conversation Search Plugin

This plugin provides semantic search across your Claude Code conversation history with progressive exploration strategies.

## Installation

### Option 1: Install from GitHub (Recommended)

Users can install directly from your GitHub repository:

```bash
# Add plugin to marketplace
/plugin marketplace add yoshi47/ai-conversation-search

# Install the plugin
/plugin install conversation-search
```

This will:
- Install the conversation-search skill
- Display installation instructions for the CLI tool

### Option 2: Manual Installation

1. Clone the repository
2. Install the CLI tool:
   ```bash
   # Download pre-built binary (see README.md for full platform detection)
   mkdir -p ~/.local/bin
   # For macOS Apple Silicon:
   curl -fsSL "https://github.com/yoshi47/ai-conversation-search/releases/latest/download/ai-conversation-search-aarch64-apple-darwin" \
       -o ~/.local/bin/ai-conversation-search && chmod +x ~/.local/bin/ai-conversation-search
   export PATH="$HOME/.local/bin:$PATH"

   # Or build from source
   cargo install --git https://github.com/yoshi47/ai-conversation-search
   ```
3. Initialize the database:
   ```bash
   ai-conversation-search init
   ```
4. Copy the skill to Claude Code:
   ```bash
   mkdir -p ~/.claude/skills/conversation-search
   cp skills/conversation-search/* ~/.claude/skills/conversation-search/
   ```

## Updates

When you publish updates to GitHub, users can update by:

```bash
/plugin update conversation-search
```

This will pull the latest skill files automatically.

## What's Included

- **Skill**: conversation-search (with progressive search workflow)
- **CLI Tool**: ai-conversation-search command-line interface
- **Database**: Local SQLite index of conversations

## Requirements

- Claude Code
- macOS (arm64 / x86_64) or Linux (x86_64)
