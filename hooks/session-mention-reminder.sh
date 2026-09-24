#!/bin/sh
# UserPromptSubmit hook: when the prompt contains a session UUID together with
# a session/conversation keyword (a proxy for a past-session reference; the
# slight over-match is intentional — a false positive only injects a harmless
# reminder), inject a reminder to use the conversation-search skill instead of
# manually grepping ~/.claude/projects/**/*.jsonl.
#
# Description tuning alone is probabilistic; this hook makes the trigger
# deterministic for the highest-precision case (raw UUID in the prompt).
# See tests/skill-discovery/scenarios.md Scenario 6 for the incident that
# motivated it.
#
# Design: fail-open. Every failure path exits 0 with no stdout so the user's
# prompt is never blocked. stderr is invisible to users on exit 0 (shown only
# in `claude --debug`), so breadcrumbs there are free.

command -v jq >/dev/null 2>&1 || {
    echo "session-mention-reminder: jq not found, hook disabled" >&2
    exit 0
}

# jq stderr is left visible on purpose: unparseable stdin means the hook input
# contract changed, and `claude --debug` is the only place that would show it.
PROMPT=$(jq -r '.prompt // empty') || exit 0
[ -n "$PROMPT" ] || exit 0

# Only inspect .prompt — the raw hook input also contains session_id and
# transcript_path, which always embed the current session's UUID and would
# always match.
# `|| exit 0` also swallows grep errors (exit 2), not just no-match (exit 1);
# with a static pattern on stdin that is practically unreachable, and fail-open
# is the right direction for it anyway.
UUID_RE='[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}'
printf '%s' "$PROMPT" | grep -qE "$UUID_RE" || exit 0
printf '%s' "$PROMPT" | grep -qiE 'session|セッション|会話|conversation|resume|続き|過去' || exit 0

cat <<'EOF'
{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"The prompt contains a session UUID and references a past session. Use the conversation-search skill to read that session — do not locate or read transcripts manually (e.g. ~/.claude/projects, ~/.codex/sessions, the OpenCode database) with find/grep/jq/sqlite, even if the path looks obvious, and even if the session is only input to another task."}}
EOF
exit 0
