#!/bin/bash
# Bump version in all required files
# Usage: ./scripts/bump-version.sh 0.7.0

set -e

if [ -z "$1" ]; then
    CURRENT=$(grep -m1 '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
    echo "Current version: $CURRENT"
    echo "Usage: $0 <new-version>"
    echo "Example: $0 0.7.0"
    exit 1
fi

NEW_VERSION="$1"

# Portable sed -i (works with both GNU sed and BSD sed)
sedi() {
    if sed --version 2>/dev/null | grep -q 'GNU sed'; then
        sed -i "$@"
    else
        sed -i '' "$@"
    fi
}

# Update Cargo.toml
sedi "s/^version = \".*\"/version = \"$NEW_VERSION\"/" Cargo.toml

# Update Cargo.lock (cargo check only updates our version, preserving dep versions)
cargo check --quiet 2>/dev/null || true

# Update plugin.json
sedi "s/\"version\": \".*\"/\"version\": \"$NEW_VERSION\"/" .claude-plugin/plugin.json

# Update marketplace.json
sedi "s/\"version\": \".*\"/\"version\": \"$NEW_VERSION\"/" .claude-plugin/marketplace.json

# Update bin/ wrapper script
sedi "s/^ACS_WRAPPER_VERSION=\".*\"/ACS_WRAPPER_VERSION=\"$NEW_VERSION\"/" bin/ai-conversation-search

echo "Updated version to $NEW_VERSION in:"
echo "  - Cargo.toml"
echo "  - Cargo.lock"
echo "  - .claude-plugin/plugin.json"
echo "  - .claude-plugin/marketplace.json"
echo "  - bin/ai-conversation-search"

# Verify
echo ""
echo "Verification:"
grep -n "version.*$NEW_VERSION" Cargo.toml .claude-plugin/*.json bin/ai-conversation-search
