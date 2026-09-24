#!/bin/sh
# Check that every version location matches Cargo.toml (see scripts/bump-version.sh).
# Usage: ./scripts/check-versions.sh [expected-version]

set -eu

CARGO=$(grep -m1 '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
EXPECTED="${1:-$CARGO}"
[ -n "$EXPECTED" ] || { echo "::error::no version found in Cargo.toml"; exit 1; }
FAILED=0

check() {
    if [ "$2" != "$EXPECTED" ]; then
        echo "::error::$1 has version '$2', expected '$EXPECTED'"
        FAILED=1
    fi
}

json_versions() {
    grep -o '"version": *"[^"]*"' "$1" | sed 's/"version": *"\(.*\)"/\1/'
}

check Cargo.toml "$CARGO"
for f in .claude-plugin/plugin.json .claude-plugin/marketplace.json skills/index.json; do
    VERSIONS=$(json_versions "$f" || true)
    [ -n "$VERSIONS" ] || check "$f" "(none)"
    for v in $VERSIONS; do
        check "$f" "$v"
    done
done
check bin/ai-conversation-search \
    "$(grep -m1 '^ACS_WRAPPER_VERSION=' bin/ai-conversation-search | sed 's/ACS_WRAPPER_VERSION="\(.*\)"/\1/')"

[ "$FAILED" -eq 0 ] && echo "All versions match: $EXPECTED"
exit "$FAILED"
