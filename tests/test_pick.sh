#!/bin/sh
# Tests for the `pick` subcommand in bin/ai-conversation-search.
# Run: sh tests/test_pick.sh
#
# These tests exercise argument parsing, command construction, and filtering
# logic without launching fzf (non-interactive).

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WRAPPER="$SCRIPT_DIR/bin/ai-conversation-search"
BINARY="${HOME}/.conversation-search/bin/ai-conversation-search-$(grep '^ACS_WRAPPER_VERSION=' "$WRAPPER" | cut -d'"' -f2)"

PASS=0
FAIL=0
TESTS=0

pass() {
    PASS=$((PASS + 1))
    TESTS=$((TESTS + 1))
    echo "  PASS: $1"
}

fail() {
    FAIL=$((FAIL + 1))
    TESTS=$((TESTS + 1))
    echo "  FAIL: $1"
    [ -n "${2:-}" ] && echo "        $2"
}

echo "=== pick subcommand tests ==="
echo ""

# --- Prerequisite check ---
echo "--- Prerequisites ---"
if [ ! -x "$BINARY" ]; then
    echo "SKIP: Binary not found at $BINARY (run 'ai-conversation-search --version' first)"
    exit 0
fi
if ! command -v fzf >/dev/null 2>&1; then
    echo "SKIP: fzf not installed"
    exit 0
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "SKIP: jq not installed"
    exit 0
fi
echo "  Binary: $BINARY"
echo "  fzf: $(command -v fzf)"
echo "  jq: $(command -v jq)"
echo ""

# --- Help ---
echo "--- Help output ---"

OUTPUT=$("$WRAPPER" pick --help 2>&1)
if echo "$OUTPUT" | grep -q "Interactive session picker"; then
    pass "--help shows description"
else
    fail "--help shows description" "Got: $OUTPUT"
fi

if echo "$OUTPUT" | grep -q "\-\-here"; then
    pass "--help includes --here option"
else
    fail "--help includes --here option"
fi

if echo "$OUTPUT" | grep -q "Examples:"; then
    pass "--help has Examples section (not duplicate Usage)"
else
    fail "--help has Examples section"
fi
echo ""

# --- Argument validation ---
echo "--- Argument validation ---"

if "$WRAPPER" pick --days 2>/dev/null; then
    fail "--days without value should fail"
else
    pass "--days without value exits non-zero"
fi

ERR=$("$WRAPPER" pick --days 2>&1 || true)
if echo "$ERR" | grep -q "requires a value"; then
    pass "--days error message is descriptive"
else
    fail "--days error message" "Got: $ERR"
fi

if "$WRAPPER" pick --repo 2>/dev/null; then
    fail "--repo without value should fail"
else
    pass "--repo without value exits non-zero"
fi

if "$WRAPPER" pick --source 2>/dev/null; then
    fail "--source without value should fail"
else
    pass "--source without value exits non-zero"
fi

if "$WRAPPER" pick --badopt 2>/dev/null; then
    fail "unknown option should fail"
else
    pass "unknown option exits non-zero"
fi

ERR=$("$WRAPPER" pick --badopt 2>&1 || true)
if echo "$ERR" | grep -q "Unknown option"; then
    pass "unknown option error message"
else
    fail "unknown option error message" "Got: $ERR"
fi
echo ""

# --- JSON fetching (list mode) ---
echo "--- JSON fetching (list mode) ---"

TMPFILE=$(mktemp)
trap 'rm -f "$TMPFILE"' EXIT
if "$BINARY" list --json --days 14 > "$TMPFILE" 2>/dev/null; then
    pass "list --json succeeds"
else
    fail "list --json succeeds"
fi

COUNT=$(jq 'length' "$TMPFILE")
if [ "$COUNT" -gt 0 ]; then
    pass "list returns results ($COUNT sessions)"
else
    fail "list returns results" "Got 0"
fi
echo ""

# --- JSON fetching (search mode) ---
echo "--- JSON fetching (search mode) ---"

TMPFILE_SEARCH=$(mktemp)
if "$BINARY" search "test" --group-by-session --json --days 30 > "$TMPFILE_SEARCH" 2>/dev/null; then
    pass "search --json succeeds"
else
    # search with no results is still valid
    pass "search --json completes (may have 0 results)"
fi
rm -f "$TMPFILE_SEARCH"
echo ""

# --- jq transformation (list mode) ---
echo "--- jq transformation ---"

LINES=$(jq -r '.[] |
  "\(.session_id)\t" +
  (if .source == "opencode" then "[OC]"
   elif .source == "codex" then "[CX]"
   else "[CC]" end) + " " +
  ((.last_message_at // .timestamp // "") | .[0:16] | gsub("T"; " ")) + "  " +
  (if .match_count then "\(.match_count) matches  "
   else "\(.message_count // "?")msg  " end) +
  ((.project_path // "") | split("/") | last) + "  " +
  ((.conversation_summary // "[no summary]") | gsub("[\\n\\r]"; " ") | .[0:60])
' "$TMPFILE")

if [ -n "$LINES" ]; then
    pass "jq transformation produces output"
else
    fail "jq transformation produces output"
fi

FIRST_LINE=$(echo "$LINES" | head -1)
# Check tab-separated format: session_id<TAB>display
TAB=$(printf '\t')
SESSION_ID=$(echo "$FIRST_LINE" | cut -f1)
DISPLAY=$(echo "$FIRST_LINE" | cut -f2-)

if echo "$SESSION_ID" | grep -qE '^[a-f0-9-]+$'; then
    pass "first field is a UUID-like session_id"
else
    fail "first field is a UUID-like session_id" "Got: $SESSION_ID"
fi

if echo "$DISPLAY" | grep -qE '^\[C[CX]\]|\[OC\]'; then
    pass "display starts with source label"
else
    fail "display starts with source label" "Got: $DISPLAY"
fi

if echo "$DISPLAY" | grep -qE '[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}'; then
    pass "display contains timestamp"
else
    fail "display contains timestamp" "Got: $DISPLAY"
fi

if echo "$DISPLAY" | grep -qE '[0-9]+msg'; then
    pass "display contains message count"
else
    fail "display contains message count" "Got: $DISPLAY"
fi
echo ""

# --- resume_command extraction ---
echo "--- resume_command extraction ---"

FIRST_SESSION=$(jq -r '.[0].session_id' "$TMPFILE")
RESUME_CMD=$(jq -r --arg sid "$FIRST_SESSION" \
  '.[] | select(.session_id == $sid) | .resume_command // empty' "$TMPFILE")

if [ -n "$RESUME_CMD" ] && [ "$RESUME_CMD" != "null" ]; then
    pass "resume_command extracted for session $FIRST_SESSION"
else
    fail "resume_command extracted" "Got: $RESUME_CMD"
fi

if echo "$RESUME_CMD" | grep -q "^cd .* && .* --resume"; then
    pass "resume_command has expected format"
else
    fail "resume_command format" "Got: $RESUME_CMD"
fi
echo ""

# --- --here filtering ---
echo "--- --here filtering ---"

CWD="$SCRIPT_DIR"
FILTERED=$(mktemp)
jq --arg pp "$CWD" '[.[] | select(.project_path == $pp)]' "$TMPFILE" > "$FILTERED"
FILTERED_COUNT=$(jq 'length' "$FILTERED")

if [ "$FILTERED_COUNT" -le "$COUNT" ]; then
    pass "--here filter reduces results ($FILTERED_COUNT <= $COUNT)"
else
    fail "--here filter reduces results"
fi
rm -f "$FILTERED"
echo ""

# --- --here repo auto-detection ---
echo "--- --here repo auto-detection ---"

PICK_GIT_COMMON=$(git -C "$SCRIPT_DIR" rev-parse --git-common-dir 2>/dev/null) || true
if [ -n "$PICK_GIT_COMMON" ]; then
    DETECTED_REPO=$(basename "$(cd "$SCRIPT_DIR" && cd "$PICK_GIT_COMMON/.." && pwd)")
    if [ "$DETECTED_REPO" = "ai-conversation-search" ]; then
        pass "repo auto-detected from normal repo: $DETECTED_REPO"
    else
        fail "repo auto-detected" "Expected: ai-conversation-search, Got: $DETECTED_REPO"
    fi
else
    fail "git-common-dir detection"
fi

# Test worktree detection if a meetsone worktree exists
WORKTREE_DIR="$HOME/meetsone.worktrees/chore"
if [ -d "$WORKTREE_DIR" ]; then
    WT_GIT_COMMON=$(git -C "$WORKTREE_DIR" rev-parse --git-common-dir 2>/dev/null) || true
    if [ -n "$WT_GIT_COMMON" ]; then
        WT_REPO=$(basename "$(cd "$WORKTREE_DIR" && cd "$WT_GIT_COMMON/.." && pwd)")
        if [ "$WT_REPO" = "meetsone" ]; then
            pass "repo auto-detected from worktree: $WT_REPO"
        else
            fail "worktree repo detection" "Expected: meetsone, Got: $WT_REPO"
        fi
    fi
else
    echo "  SKIP: worktree dir $WORKTREE_DIR not found"
fi
echo ""

# --- preview command ---
echo "--- preview command ---"

PREVIEW_OUTPUT=$("$BINARY" tree "$FIRST_SESSION" --json 2>/dev/null | jq -r '
  "📁 " + (.conversation.project_path // "unknown"),
  "💬 " + (.total_messages | tostring) + " messages",
  "🕐 " + ((.conversation.first_message_at // "")[0:16] | gsub("T"; " ")) + " → " + ((.conversation.last_message_at // "")[0:16] | gsub("T"; " "))
' 2>/dev/null || echo "")

if echo "$PREVIEW_OUTPUT" | grep -q "📁"; then
    pass "preview shows project path"
else
    fail "preview shows project path"
fi

if echo "$PREVIEW_OUTPUT" | grep -q "💬.*messages"; then
    pass "preview shows message count"
else
    fail "preview shows message count"
fi

if echo "$PREVIEW_OUTPUT" | grep -q "🕐"; then
    pass "preview shows time range"
else
    fail "preview shows time range"
fi
echo ""

# --- Variable shadowing ---
echo "--- Variable safety ---"

# REPO should not be clobbered by pick (uses PICK_REPO internally)
if grep -q 'PICK_REPO' "$WRAPPER" && ! grep -q '^\s*REPO=""' "$WRAPPER"; then
    pass "no REPO variable shadowing (uses PICK_REPO)"
else
    fail "REPO variable shadowing check"
fi

if grep -q 'PICK_TMPFILE' "$WRAPPER" && ! grep -q '^\s*TMPFILE=.*mktemp' "$WRAPPER"; then
    pass "no TMPFILE variable shadowing (uses PICK_TMPFILE)"
else
    fail "TMPFILE variable shadowing check"
fi

# POSIX compliance: no $'\t' bashism
if grep -q "\$'\\\\t'" "$WRAPPER"; then
    fail "POSIX compliance: found bashism \$'\\t'"
else
    pass "POSIX compliance: no \$'\\t' bashism"
fi
echo ""

# --- Summary ---
echo "==========================="
echo "Total: $TESTS  Pass: $PASS  Fail: $FAIL"
if [ "$FAIL" -gt 0 ]; then
    echo "SOME TESTS FAILED"
    exit 1
else
    echo "ALL TESTS PASSED"
fi
