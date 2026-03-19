use clap::{Parser, Subcommand};

use crate::db;
use crate::error::Result;
use crate::indexer::ConversationIndexer;
use crate::indexer::codex::CodexIndexer;
use crate::indexer::opencode::{OpenCodeIndexer, get_opencode_db_path};
use crate::search::{ConversationSearch, TreeNode, format_timestamp};

/// Source display labels
const SOURCE_LABELS: &[(&str, &str)] = &[("opencode", "[OC]"), ("codex", "[CX]")];
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
    /// Search conversations
    Search {
        /// Search query
        query: String,
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
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Skip auto-indexing
        #[arg(long)]
        no_index: bool,
        /// Force re-index (ignore TTL cooldown)
        #[arg(long)]
        force_index: bool,
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
        /// Skip auto-indexing
        #[arg(long)]
        no_index: bool,
        /// Force re-index (ignore TTL cooldown)
        #[arg(long)]
        force_index: bool,
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
        /// Skip auto-indexing
        #[arg(long)]
        no_index: bool,
        /// Force re-index (ignore TTL cooldown)
        #[arg(long)]
        force_index: bool,
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
        let mut oc = OpenCodeIndexer::new(None, None, quiet);
        let _ = oc.scan_and_index(days_back);
    }

    let codex_dir = db::expand_path("~/.codex/sessions");
    if codex_dir.exists() {
        if !quiet {
            eprintln!("\nIndexing Codex CLI conversations...");
        }
        let mut cx = CodexIndexer::new(None, None, quiet);
        let _ = cx.scan_and_index(days_back);
    }
}

fn touch_stamp_file() {
    let stamp_file = db::expand_path("~/.conversation-search/.last-auto-index");
    let _ = std::fs::write(&stamp_file, "");
}

fn auto_index(days_back: i64, force: bool) {
    // TTL cooldown: skip if recently indexed (unless forced)
    let stamp_file = db::expand_path("~/.conversation-search/.last-auto-index");
    let ttl_secs = match std::env::var("CONVERSATION_SEARCH_INDEX_TTL") {
        Ok(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("Warning: invalid CONVERSATION_SEARCH_INDEX_TTL='{}', using default 60s", v);
                60
            }
        },
        Err(_) => 60,
    };

    if !force {
        if let Ok(Ok(mtime)) = std::fs::metadata(&stamp_file).map(|m| m.modified()) {
            if mtime.elapsed().unwrap_or_default() < std::time::Duration::from_secs(ttl_secs) {
                return;
            }
        }
    }

    let mut indexer = match ConversationIndexer::new(db::DEFAULT_DB_PATH, true) {
        Ok(i) => i,
        Err(_) => return,
    };

    if force {
        indexer.clear_sync_state();
    }

    let files = indexer.scan_conversations(Some(days_back));
    for conv_file in &files {
        let _ = indexer.index_conversation(conv_file);
    }

    index_other_sources(Some(days_back), true);
    touch_stamp_file();
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
        Some(Commands::Search {
            query,
            days,
            since,
            until,
            date,
            project,
            repo,
            source,
            limit,
            content,
            json,
            no_index,
            force_index,
        }) => cmd_search(
            &query, days, since, until, date, project, repo, source, limit, content, json,
            no_index, force_index,
        ),
        Some(Commands::Context {
            uuid,
            depth,
            content,
            json,
            no_index,
            force_index,
        }) => cmd_context(&uuid, depth, content, json, no_index, force_index),
        Some(Commands::List {
            days,
            since,
            until,
            date,
            limit,
            repo,
            source,
            json,
            no_index,
            force_index,
        }) => cmd_list(days, since, until, date, limit, repo, source, json, no_index, force_index),
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

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_search(
    query: &str,
    days: Option<i64>,
    since: Option<String>,
    until: Option<String>,
    date: Option<String>,
    project: Option<String>,
    repo: Option<String>,
    source: Option<String>,
    limit: i64,
    show_content: bool,
    json_output: bool,
    no_index: bool,
    force_index: bool,
) -> Result<()> {
    if !no_index {
        auto_index(days.unwrap_or(30), force_index);
    }

    let mut search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;

    let results = search.search_conversations(
        query,
        days,
        since.as_deref(),
        until.as_deref(),
        date.as_deref(),
        limit,
        project.as_deref(),
        repo.as_deref(),
        128,
        source.as_deref(),
    )?;

    if json_output {
        let json_val: serde_json::Value = serde_json::to_value(&results)?;
        let localized = localize_timestamps(json_val);
        println!("{}", serde_json::to_string_pretty(&localized)?);
        return Ok(());
    }

    if results.is_empty() {
        println!("No results found for: {}", query);
        return Ok(());
    }

    println!("\u{1f50d} Found {} matches for '{}':\n", results.len(), query);

    for result in &results {
        let icon = if result.message_type == "user" { "\u{1f464}" } else { "\u{1f916}" };
        let timestamp = format_timestamp(&result.timestamp, true, false);
        let source_str = result.source.as_deref().unwrap_or("claude_code");
        let label = source_label(source_str);

        let project_dir = result.project_path.as_deref().unwrap_or("");
        let summary = result.conversation_summary.as_deref().unwrap_or("");
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

    Ok(())
}

fn cmd_context(uuid: &str, depth: i32, show_content: bool, json_output: bool, no_index: bool, force_index: bool) -> Result<()> {
    if !no_index {
        auto_index(30, force_index);
    }

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

#[allow(clippy::too_many_arguments)]
fn cmd_list(
    days: Option<i64>,
    since: Option<String>,
    until: Option<String>,
    date: Option<String>,
    limit: i64,
    repo: Option<String>,
    source: Option<String>,
    json_output: bool,
    no_index: bool,
    force_index: bool,
) -> Result<()> {
    if !no_index {
        auto_index(days.unwrap_or(30), force_index);
    }

    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    let convs = search.list_recent_conversations(
        days,
        since.as_deref(),
        until.as_deref(),
        date.as_deref(),
        limit,
        None,
        repo.as_deref(),
        source.as_deref(),
    )?;

    if json_output {
        let json_val = serde_json::to_value(&convs)?;
        let localized = localize_timestamps(json_val);
        println!("{}", serde_json::to_string_pretty(&localized)?);
        return Ok(());
    }

    if convs.is_empty() {
        println!("No conversations found");
        return Ok(());
    }

    let display_days = days.unwrap_or(7);
    println!("Recent conversations (last {} days):\n", display_days);

    for conv in &convs {
        let last_at = conv.last_message_at.as_deref().unwrap_or("");
        let timestamp = format_timestamp(last_at, true, false);
        let source_str = conv.source.as_deref().unwrap_or("claude_code");
        let label = source_label(source_str);
        let summary = conv.conversation_summary.as_deref().unwrap_or("");
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
