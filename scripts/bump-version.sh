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

# Cross-platform sed -i
sedi() {
    if [[ "$OSTYPE" == "darwin"* ]]; then
        sed -i '' "$@"
    else
        sed -i "$@"
    fi
}

# Update Cargo.toml
sedi "s/^version = \".*\"/version = \"$NEW_VERSION\"/" Cargo.toml

# Update Cargo.lock
cargo generate-lockfile 2>/dev/null || true

# Update plugin.json
sedi "s/\"version\": \".*\"/\"version\": \"$NEW_VERSION\"/" .claude-plugin/plugin.json

# Update marketplace.json
sedi "s/\"version\": \".*\"/\"version\": \"$NEW_VERSION\"/" .claude-plugin/marketplace.json

echo "Updated version to $NEW_VERSION in:"
echo "  - Cargo.toml"
echo "  - Cargo.lock"
echo "  - .claude-plugin/plugin.json"
echo "  - .claude-plugin/marketplace.json"

# Verify
echo ""
echo "Verification:"
grep -n "version.*$NEW_VERSION" Cargo.toml .claude-plugin/*.json
