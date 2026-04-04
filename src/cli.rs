use clap::{Parser, Subcommand};

use crate::db;
use crate::error::Result;
use crate::indexer::ConversationIndexer;
use crate::indexer::codex::CodexIndexer;
use crate::indexer::opencode::{OpenCodeIndexer, get_opencode_db_path};
use crate::indexer::count_conversation_files_on_disk;
use crate::search::{ConversationSearch, SearchFilter, TreeNode, format_timestamp};

/// Source display labels
const SOURCE_LABELS: &[(&str, &str)] = &[("opencode", "[OC]"), ("codex", "[CX]")];

const AUTO_INDEX_TTL_SECS: u64 = 300;
const FULL_INDEX_TTL_SECS: u64 = 86400;
const STAMP_FILE_PATH: &str = "~/.conversation-search/.last-auto-index";
const FULL_STAMP_FILE_PATH: &str = "~/.conversation-search/.last-full-index";

fn source_label(source: &str) -> &str {
    SOURCE_LABELS
        .iter()
        .find(|(k, _)| *k == source)
        .map(|(_, v)| *v)
        .unwrap_or("[CC]")
}

fn claude_cmd() -> String {
    std::env::var("CC_CONVERSATION_SEARCH_CMD").unwrap_or_else(|_| "claude".to_string())
}

/// Recursively convert UTC ISO timestamps to local timezone in JSON values.
fn localize_timestamps(val: serde_json::Value) -> serde_json::Value {
    use chrono::DateTime;

    match val {
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(localize_timestamps).collect())
        }
        serde_json::Value::Object(map) => {
            let timestamp_keys = ["timestamp", "first_message_at", "last_message_at", "indexed_at"];
            let new_map: serde_json::Map<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| {
                    if timestamp_keys.contains(&k.as_str()) {
                        if let Some(s) = v.as_str() {
                            if s.ends_with('Z') {
                                let cleaned = s.replace('Z', "+00:00");
                                if let Ok(dt) = DateTime::parse_from_rfc3339(&cleaned) {
                                    let local: DateTime<chrono::Local> = dt.with_timezone(&chrono::Local);
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
#[command(name = "ai-conversation-search", about = "Find and resume Claude Code conversations using semantic search")]
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
        /// Minimal output
        #[arg(long)]
        quiet: bool,
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
        /// Show full content
        #[arg(long)]
        content: bool,
        /// Show search diagnostics (session/message counts)
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Group results by session (show best match per session)
        #[arg(long)]
        group_by_session: bool,
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
            Ok(mtime) => mtime.elapsed().unwrap_or_default() >= std::time::Duration::from_secs(ttl_secs),
            Err(_) => true,
        },
        Err(_) => true,
    }
}

/// Spawn a background index process if the stamp file is stale.
/// All errors are silently ignored — this must never block or fail the caller.
fn maybe_background_index() {
    let _ = try_background_index();
}

fn try_background_index() -> Option<()> {
    let stamp_path = db::expand_path(STAMP_FILE_PATH);
    let full_stamp_path = db::expand_path(FULL_STAMP_FILE_PATH);

    let ttl_secs = std::env::var("CONVERSATION_SEARCH_INDEX_TTL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(AUTO_INDEX_TTL_SECS);

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

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => {
            // Print help
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            std::process::exit(1);
        }
        Some(Commands::Init {
            days,
            force,
            quiet,
        }) => cmd_init(days, force, quiet),
        Some(Commands::Index {
            days,
            all,
            quiet,
        }) => cmd_index(days, all, quiet),
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
            verbose,
            group_by_session,
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
            };
            cmd_search(&effective_query, &filter, content, verbose, group_by_session, json)
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
            };
            cmd_list(&filter, json)
        }
        Some(Commands::Tree { session_id, json }) => cmd_tree(&session_id, json),
        Some(Commands::Resume { uuid }) => cmd_resume(&uuid),
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
                eprint!("  [{}/{}] {}\r", i + 1, total, conv_file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
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

fn cmd_index(days: i64, all: bool, quiet: bool) -> Result<()> {
    let mut indexer = ConversationIndexer::new(db::DEFAULT_DB_PATH, quiet)?;

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
                eprint!("[{}/{}] {}\r", i + 1, total, conv_file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
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
    println!("FTS health: {}", if status.fts_healthy { "OK" } else { "CORRUPTED" });

    println!("\nSessions: {} total", status.total_conversations);
    for sc in &status.by_source {
        println!("  {}: {}", sc.source, sc.count);
    }

    println!("\nMessages: {} total", status.total_messages);

    if let (Some(ref earliest), Some(ref latest)) = (&status.earliest_conversation, &status.latest_conversation) {
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

    println!("\nIndexed files: {} / {} on disk", status.indexed_files, status.files_on_disk);
    let unindexed = status.files_on_disk as i64 - status.indexed_files;
    if unindexed > 0 {
        eprintln!("  \u{26a0} {} files not indexed. Run 'ai-conversation-search index --all' to include them.", unindexed);
    }

    Ok(())
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

fn inject_resume_command(val: &mut serde_json::Value) {
    let cmd = claude_cmd();
    match val {
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                inject_resume_command(item);
            }
        }
        serde_json::Value::Object(map) => {
            // Only inject into objects that have session_id (i.e., result rows)
            if map.contains_key("session_id") {
                let source = map.get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("claude_code")
                    .to_string();
                let session_id = map.get("session_id").and_then(|v| v.as_str()).map(String::from);
                let project_path = map.get("project_path").and_then(|v| v.as_str()).map(String::from);

                let resume = match (session_id, project_path) {
                    (Some(sid), Some(pp)) => match source.as_str() {
                        "opencode" | "codex" => serde_json::Value::Null,
                        _ => serde_json::Value::String(format!("cd {} && {} --resume {}", pp, cmd, sid)),
                    },
                    _ => serde_json::Value::Null,
                };
                map.insert("resume_command".to_string(), resume);
            }
        }
        _ => {}
    }
}

fn display_summary(summary: Option<&str>) -> &str {
    match summary {
        Some(s) if !s.is_empty() => s,
        _ => "[no summary]",
    }
}

fn cmd_search(
    query: &str,
    filter: &SearchFilter<'_>,
    show_content: bool,
    verbose: bool,
    group_by_session: bool,
    json_output: bool,
) -> Result<()> {
    maybe_background_index();
    let mut search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;

    let search_result = search.search_conversations(query, filter)?;
    let results = search_result.rows;
    let stats = &search_result.stats;

    if json_output {
        let json_results = if group_by_session {
            let grouped = group_results_by_session(&results);
            let mut arr = Vec::new();
            for (representative, count) in &grouped {
                let mut obj = serde_json::to_value(representative)?;
                if let Some(map) = obj.as_object_mut() {
                    map.insert("match_count".to_string(), serde_json::Value::Number((*count).into()));
                }
                arr.push(obj);
            }
            serde_json::Value::Array(arr)
        } else {
            serde_json::to_value(&results)?
        };
        let mut localized = localize_timestamps(json_results);
        inject_resume_command(&mut localized);
        println!("{}", serde_json::to_string_pretty(&localized)?);
        if verbose {
            eprintln!("Scanned {} sessions ({} messages), {} matched",
                stats.sessions_in_scope, stats.total_indexed_messages, stats.matched_messages);
            print_unindexed_warning(&search);
        }
        return Ok(());
    }

    if results.is_empty() {
        println!("No results found for: {}", query);
        eprintln!("Scanned {} sessions ({} messages), 0 matched",
            stats.sessions_in_scope, stats.total_indexed_messages);
        print_unindexed_warning(&search);
        return Ok(());
    }

    if verbose {
        eprintln!("Scanned {} sessions ({} messages), {} matched",
            stats.sessions_in_scope, stats.total_indexed_messages, stats.matched_messages);
        print_unindexed_warning(&search);
    }

    if group_by_session {
        let grouped = group_results_by_session(&results);
        println!("\u{1f50d} Found {} matches across {} sessions for '{}':\n",
            results.len(), grouped.len(), query);

        for (result, match_count) in &grouped {
            let source_str = result.source.as_deref().unwrap_or("claude_code");
            let label = source_label(source_str);
            let summary = display_summary(result.conversation_summary.as_deref());
            let project_dir = result.project_path.as_deref().unwrap_or("");
            let session_id = &result.session_id;
            let timestamp = format_timestamp(&result.timestamp, true, false);

            println!("{} {} ({} matches)", label, summary, match_count);
            println!("   Session: {}", session_id);
            println!("   Project: {}", project_dir);
            println!("   Time: {}", timestamp);
            println!("\n   {}", result.context_snippet);

            if source_str != "opencode" && source_str != "codex" {
                println!("\n   Resume:");
                println!("     cd {}", project_dir);
                println!("     {} --resume {}", claude_cmd(), session_id);
            }
            println!();
        }
    } else {
        println!("\u{1f50d} Found {} matches for '{}':\n", results.len(), query);

        for result in &results {
            let icon = if result.message_type == "user" { "\u{1f464}" } else { "\u{1f916}" };
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
                    let truncated: String = content.chars().take(300).collect();
                    println!("\n   {}...", truncated);
                }
            } else {
                println!("\n   {}", result.context_snippet);
            }

            if source_str == "opencode" {
                println!("\n   OpenCode session: {}", session_id.strip_prefix("oc:").unwrap_or(session_id));
            } else if source_str == "codex" {
                println!("\n   Codex session: {}", session_id.strip_prefix("codex:").unwrap_or(session_id));
            } else {
                println!("\n   Resume:");
                println!("     cd {}", project_dir);
                println!("     {} --resume {}", claude_cmd(), session_id);
            }
            println!();
        }
    }

    Ok(())
}

/// Group search results by session_id, preserving order of first occurrence.
fn group_results_by_session(results: &[crate::search::SearchResultRow]) -> Vec<(&crate::search::SearchResultRow, usize)> {
    let mut seen: std::collections::HashMap<&str, (usize, usize)> = std::collections::HashMap::new(); // session_id -> (first_index, count)
    let mut order: Vec<&str> = Vec::new();

    for (i, result) in results.iter().enumerate() {
        let sid = result.session_id.as_str();
        seen.entry(sid)
            .and_modify(|(_, count)| *count += 1)
            .or_insert_with(|| {
                order.push(sid);
                (i, 1)
            });
    }

    order.iter().map(|sid| {
        let (idx, count) = seen[sid];
        (&results[idx], count)
    }).collect()
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
            let icon = if msg.message_type == "user" { "\u{1f464}" } else { "\u{1f916}" };
            let summary = msg.summary.as_deref().unwrap_or("No summary");
            println!("  {} {}", icon, summary);
        }
        println!();
    }

    // Show target
    if let Some(ref msg) = result.message {
        println!("\u{1f3af} Target message:");
        let icon = if msg.message_type == "user" { "\u{1f464}" } else { "\u{1f916}" };
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

fn cmd_list(
    filter: &SearchFilter<'_>,
    json_output: bool,
) -> Result<()> {
    maybe_background_index();
    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    let convs = search.list_recent_conversations(filter)?;

    if json_output {
        let json_val = serde_json::to_value(&convs)?;
        let mut localized = localize_timestamps(json_val);
        inject_resume_command(&mut localized);
        println!("{}", serde_json::to_string_pretty(&localized)?);
        return Ok(());
    }

    if convs.is_empty() {
        println!("No conversations found");
        return Ok(());
    }

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
        println!("  {}", project);
        println!("  Session: {}", conv.session_id);
        println!();
    }

    Ok(())
}

fn cmd_tree(session_id: &str, json_output: bool) -> Result<()> {
    maybe_background_index();
    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    let tree = search.get_conversation_tree(session_id)?;

    if json_output {
        let json_val = serde_json::to_value(&tree)?;
        let localized = localize_timestamps(json_val);
        println!("{}", serde_json::to_string_pretty(&localized)?);
        return Ok(());
    }

    println!("Conversation tree: {}\n", session_id);

    if let Some(ref err) = tree.error {
        println!("Error: {}", err);
        return Ok(());
    }

    print_tree_nodes(&tree.tree, 0);

    Ok(())
}

fn print_tree_nodes(nodes: &[TreeNode], indent: usize) {
    for node in nodes {
        let icon = if node.message_type == "user" { "\u{1f464}" } else { "\u{1f916}" };
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
            println!("cd {}", project_path);
            println!("{} --resume {}", claude_cmd(), session_id);
        }
        Err(_) => {
            eprintln!("Message not found: {}", uuid);
            std::process::exit(1);
        }
    }

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
    fn test_is_stamp_stale_missing_file() {
        let path = unique_stamp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(is_stamp_stale(&path, 300));
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
