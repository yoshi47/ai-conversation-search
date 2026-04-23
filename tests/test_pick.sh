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

# --- JSON fetching (search mode, used by pick) ---
echo "--- JSON fetching (search mode) ---"

TMPFILE=$(mktemp)
trap 'rm -f "$TMPFILE"' EXIT
if "$BINARY" search "" --group-by-session --json --days 14 > "$TMPFILE" 2>/dev/null; then
    pass "search --group-by-session --json succeeds with empty query"
else
    fail "search --json succeeds"
fi

COUNT=$(jq 'length' "$TMPFILE")
if [ "$COUNT" -gt 0 ]; then
    pass "search returns results ($COUNT sessions)"
else
    fail "search returns results" "Got 0"
fi
echo ""

# --- jq transformation (pick's 3-column format) ---
# Format: SESSION_ID<TAB>RESUME_CMD<TAB>DISPLAY
echo "--- jq transformation ---"

LINES=$(jq -r '.[] |
  "\(.session_id)\t\(.resume_command // "")\t" +
  (if .source == "opencode" then "[OC]"
   elif .source == "codex" then "[CX]"
   else "[CC]" end) + " " +
  ((.last_message_at // .timestamp // "") | .[0:16] | gsub("T"; " ")) + "  " +
  "\(.match_count // 0) matches  " +
  ((.project_path // "") | split("/") | last) + "  " +
  ((.conversation_summary // "[no summary]") | gsub("[\\n\\r]"; " ") | .[0:60])
' "$TMPFILE")

if [ -n "$LINES" ]; then
    pass "jq transformation produces output"
else
    fail "jq transformation produces output"
fi

FIRST_LINE=$(echo "$LINES" | head -1)
TAB=$(printf '\t')
SESSION_ID=$(echo "$FIRST_LINE" | cut -f1)
RESUME_FIELD=$(echo "$FIRST_LINE" | cut -f2)
DISPLAY=$(echo "$FIRST_LINE" | cut -f3)

if echo "$SESSION_ID" | grep -qE '^[a-f0-9-]+$'; then
    pass "first field is a UUID-like session_id"
else
    fail "first field is a UUID-like session_id" "Got: $SESSION_ID"
fi

# resume_command may be empty for opencode/codex sources; only assert format if non-empty
if [ -n "$RESUME_FIELD" ]; then
    if echo "$RESUME_FIELD" | grep -q "^cd .* && .* --resume "; then
        pass "second field is a valid resume command"
    else
        fail "second field is a valid resume command" "Got: $RESUME_FIELD"
    fi
else
    pass "second field is empty (opencode/codex session, expected)"
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

if echo "$DISPLAY" | grep -qE '[0-9]+ matches'; then
    pass "display contains match count"
else
    fail "display contains match count" "Got: $DISPLAY"
fi
echo ""

# --- resume_command availability ---
# With the 3-column format, pick extracts resume via `cut -f2`, but we
# still verify that the JSON itself carries resume_command for at least
# one session so downstream resume-by-cut works.
echo "--- resume_command availability ---"

RESUMABLE_COUNT=$(jq '[.[] | select(.resume_command != null and .resume_command != "")] | length' "$TMPFILE")
if [ "$RESUMABLE_COUNT" -gt 0 ]; then
    pass "$RESUMABLE_COUNT sessions carry resume_command"
else
    fail "at least one session should have resume_command" "Got 0"
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

# Pick a real session from the fetched JSON to exercise the preview path.
FIRST_SESSION=$(jq -r '.[0].session_id // empty' "$TMPFILE")
if [ -z "$FIRST_SESSION" ]; then
    echo "  SKIP: preview tests (no sessions available)"
else
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

# The pick function should not introduce a top-level TMPFILE (reserved by the
# download wrapper). Reload-based pick has no mktemp call at all.
if ! grep -q '^\s*TMPFILE=.*mktemp' "$WRAPPER"; then
    pass "no TMPFILE variable shadowing"
else
    fail "TMPFILE variable shadowing detected"
fi

# POSIX compliance: no $'\t' bashism
if grep -q "\$'\\\\t'" "$WRAPPER"; then
    fail "POSIX compliance: found bashism \$'\\t'"
else
    pass "POSIX compliance: no \$'\\t' bashism"
fi
echo ""

# --- Reload script smoke test ---
# Verify the reload flow end-to-end: extract the RELOAD_SCRIPT heredoc from
# the wrapper, invoke it against the real binary, and assert the 3-column
# output format is well-formed. This is the exact path fzf --bind triggers.
echo "--- Reload script smoke test ---"

RELOAD_SCRIPT=$(sed -n "/<<'RELOAD_EOF'/,/^RELOAD_EOF$/{//!p;}" "$WRAPPER")
if [ -z "$RELOAD_SCRIPT" ]; then
    fail "RELOAD_SCRIPT heredoc extracted from wrapper"
else
    pass "RELOAD_SCRIPT heredoc extracted from wrapper"

    export ACS_BIN="$BINARY"
    export ACS_DAYS=14
    export ACS_REPO=""
    export ACS_SOURCE=""
    export ACS_PROJECT=""
    export ACS_LIMIT=""

    # run_reload QUERY — captures output and exit code separately.
    # Without this the `|| true` pattern swallowed real failures and made
    # every "no-crash" assertion vacuously pass.
    run_reload() {
        OUT=$(sh -c "$RELOAD_SCRIPT" _ "$1" 2>/dev/null)
        RC=$?
    }

    run_reload ""
    if [ "$RC" -eq 0 ] && [ -n "$OUT" ] && echo "$OUT" | head -1 | grep -qE '^[a-f0-9-]+'"$TAB"; then
        pass "reload script produces tab-separated lines starting with UUID"
    else
        fail "reload script output format" "rc=$RC, first line: $(echo "$OUT" | head -1)"
    fi

    # Japanese body search (FTS trigram). Must exit 0; output may be empty
    # (no match) or well-formed tab-separated lines.
    run_reload "認証"
    if [ "$RC" -eq 0 ]; then
        if [ -z "$OUT" ] || echo "$OUT" | head -1 | grep -qE '^[a-f0-9-]+'"$TAB"; then
            pass "reload script handles CJK queries cleanly"
        else
            fail "CJK output format malformed" "$(echo "$OUT" | head -1)"
        fi
    else
        fail "reload script CJK query crashed" "rc=$RC"
    fi

    # FTS syntax-degrade resilience: unclosed quote must exit 0 with empty
    # or well-formed output, not crash.
    run_reload '"unclosed'
    if [ "$RC" -eq 0 ]; then
        if [ -z "$OUT" ] || echo "$OUT" | head -1 | grep -qE '^[a-f0-9-]+'"$TAB"; then
            pass "reload script handles malformed FTS input cleanly"
        else
            fail "malformed FTS output format" "$(echo "$OUT" | head -1)"
        fi
    else
        fail "reload script malformed FTS crashed" "rc=$RC"
    fi
fi
echo ""

# --- fzf contract assertions ---
# If these flags drift, the 3-column cut -f1/-f2 extraction silently breaks.
echo "--- fzf contract ---"

if grep -q -- '--with-nth=3\.\.' "$WRAPPER"; then
    pass "fzf --with-nth=3.. hides session_id + resume_cmd columns"
else
    fail "fzf --with-nth=3.. flag missing"
fi

if grep -q -- '--nth=3\.\.' "$WRAPPER"; then
    pass "fzf --nth=3.. restricts fzf-side matching (disabled mode anyway)"
else
    fail "fzf --nth=3.. flag missing"
fi

if grep -q -- '--delimiter="\$TAB"' "$WRAPPER"; then
    pass "fzf --delimiter is TAB (required for cut -f1/-f2)"
else
    fail "fzf --delimiter=TAB missing"
fi

if grep -qF 'change:reload:sh -c "$ACS_RELOAD_SCRIPT"' "$WRAPPER"; then
    pass "fzf change:reload wired to ACS_RELOAD_SCRIPT env var"
else
    fail "fzf change:reload binding missing or altered"
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
