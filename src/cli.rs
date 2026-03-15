use clap::{Parser, Subcommand};

use crate::db;
use crate::error::Result;
use crate::indexer::ConversationIndexer;
use crate::indexer::codex::CodexIndexer;
use crate::indexer::opencode::{OpenCodeIndexer, get_opencode_db_path};
use crate::search::{ConversationSearch, format_timestamp};

/// Source display labels
const SOURCE_LABELS: &[(&str, &str)] = &[("opencode", "[OC]"), ("codex", "[CX]")];
/// Sources that store real paths
const EXTERNAL_SOURCES: &[&str] = &["opencode", "codex"];

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
        /// Skip smart extraction
        #[arg(long)]
        no_extract: bool,
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
        /// Skip smart extraction
        #[arg(long)]
        no_extract: bool,
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

fn auto_index(days_back: i64) {
    let mut indexer = match ConversationIndexer::new(db::DEFAULT_DB_PATH, true) {
        Ok(i) => i,
        Err(_) => return,
    };

    let files = indexer.scan_conversations(Some(days_back));
    for conv_file in &files {
        let _ = indexer.index_conversation(conv_file);
    }

    // Index OpenCode
    let oc_path = db::expand_path(&get_opencode_db_path());
    if oc_path.exists() {
        let mut oc = OpenCodeIndexer::new(None, None, true);
        let _ = oc.scan_and_index(Some(days_back));
    }

    // Index Codex
    let codex_dir = db::expand_path("~/.codex/sessions");
    if codex_dir.exists() {
        let mut cx = CodexIndexer::new(None, None, true);
        let _ = cx.scan_and_index(Some(days_back));
    }
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
            no_extract: _,
            force,
            quiet,
        }) => cmd_init(days, force, quiet),
        Some(Commands::Index {
            days,
            all,
            no_extract: _,
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
        }) => cmd_search(
            &query, days, since, until, date, project, repo, source, limit, content, json,
            no_index,
        ),
        Some(Commands::Context {
            uuid,
            depth,
            content,
            json,
            no_index,
        }) => cmd_context(&uuid, depth, content, json, no_index),
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
        }) => cmd_list(days, since, until, date, limit, repo, source, json, no_index),
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

    // Index OpenCode
    let oc_path = db::expand_path(&get_opencode_db_path());
    if oc_path.exists() {
        if !quiet {
            eprintln!("\nIndexing OpenCode conversations...");
        }
        let mut oc = OpenCodeIndexer::new(None, None, quiet);
        let _ = oc.scan_and_index(Some(days));
    }

    // Index Codex
    let codex_dir = db::expand_path("~/.codex/sessions");
    if codex_dir.exists() {
        if !quiet {
            eprintln!("\nIndexing Codex CLI conversations...");
        }
        let mut cx = CodexIndexer::new(None, None, quiet);
        let _ = cx.scan_and_index(Some(days));
    }

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

    // OpenCode
    let oc_days = if all { Some(9999i64) } else { Some(days) };
    let oc_path = db::expand_path(&get_opencode_db_path());
    if oc_path.exists() {
        let mut oc = OpenCodeIndexer::new(None, None, quiet);
        let _ = oc.scan_and_index(oc_days);
    }

    // Codex
    let codex_dir = db::expand_path("~/.codex/sessions");
    if codex_dir.exists() {
        let mut cx = CodexIndexer::new(None, None, quiet);
        let _ = cx.scan_and_index(oc_days);
    }

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
) -> Result<()> {
    if !no_index {
        auto_index(days.unwrap_or(30));
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
        let msg_type = result.get("message_type").and_then(|v| v.as_str()).unwrap_or("");
        let icon = if msg_type == "user" { "\u{1f464}" } else { "\u{1f916}" };
        let timestamp = format_timestamp(
            result.get("timestamp").and_then(|v| v.as_str()).unwrap_or(""),
            true,
            false,
        );
        let source_str = result.get("source").and_then(|v| v.as_str()).unwrap_or("claude_code");
        let label = source_label(source_str);
        let is_external = EXTERNAL_SOURCES.contains(&source_str);

        let project_dir = if is_external {
            result.get("project_path").and_then(|v| v.as_str()).unwrap_or("").to_string()
        } else {
            let pp = result.get("project_path").and_then(|v| v.as_str()).unwrap_or("").replace('-', "/");
            if pp.starts_with('/') { pp } else { format!("/{}", pp) }
        };

        let summary = result.get("conversation_summary").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = result.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        let message_uuid = result.get("message_uuid").and_then(|v| v.as_str()).unwrap_or("");

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
            let snippet = result.get("context_snippet").and_then(|v| v.as_str()).unwrap_or("");
            println!("\n   {}", snippet);
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

fn cmd_context(uuid: &str, depth: i32, show_content: bool, json_output: bool, no_index: bool) -> Result<()> {
    if !no_index {
        auto_index(30);
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

    if let Some(err) = result.get("error") {
        println!("Error: {}", err);
        return Ok(());
    }

    // Show ancestors
    if let Some(ancestors) = result.get("ancestors").and_then(|v| v.as_array()) {
        if !ancestors.is_empty() {
            println!("\u{1f4dc} Parent messages:");
            for msg in ancestors {
                let icon = if msg.get("message_type").and_then(|v| v.as_str()) == Some("user") {
                    "\u{1f464}"
                } else {
                    "\u{1f916}"
                };
                let summary = msg.get("summary").and_then(|v| v.as_str()).unwrap_or("No summary");
                println!("  {} {}", icon, summary);
            }
            println!();
        }
    }

    // Show target
    if let Some(msg) = result.get("message") {
        println!("\u{1f3af} Target message:");
        let icon = if msg.get("message_type").and_then(|v| v.as_str()) == Some("user") {
            "\u{1f464}"
        } else {
            "\u{1f916}"
        };
        if show_content {
            let content = msg.get("full_content").and_then(|v| v.as_str()).unwrap_or("No content");
            println!("  {} {}", icon, content);
        } else {
            let summary = msg.get("summary").and_then(|v| v.as_str()).unwrap_or("No summary");
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
) -> Result<()> {
    if !no_index {
        auto_index(days.unwrap_or(30));
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
        let last_at = conv.get("last_message_at").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp = format_timestamp(last_at, true, false);
        let source_str = conv.get("source").and_then(|v| v.as_str()).unwrap_or("claude_code");
        let label = source_label(source_str);
        let summary = conv.get("conversation_summary").and_then(|v| v.as_str()).unwrap_or("");
        let msg_count = conv.get("message_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let project = conv.get("project_path").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = conv.get("session_id").and_then(|v| v.as_str()).unwrap_or("");

        println!("{} [{}] {}", label, timestamp, summary);
        println!("  {} messages", msg_count);
        println!("  {}", project);
        println!("  Session: {}", session_id);
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

    if let Some(err) = tree.get("error") {
        println!("Error: {}", err);
        return Ok(());
    }

    if let Some(nodes) = tree.get("tree").and_then(|v| v.as_array()) {
        print_tree_nodes(nodes, 0);
    }

    Ok(())
}

fn print_tree_nodes(nodes: &[serde_json::Value], indent: usize) {
    for node in nodes {
        let icon = if node.get("message_type").and_then(|v| v.as_str()) == Some("user") {
            "\u{1f464}"
        } else {
            "\u{1f916}"
        };
        let summary = node
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let truncated: String = summary.chars().take(80).collect();
        let prefix = "  ".repeat(indent);
        println!("{}{} {}", prefix, icon, truncated);
        if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
            print_tree_nodes(children, indent + 1);
        }
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
            let mut project_dir = project_path.replace('-', "/");
            if !project_dir.starts_with('/') {
                project_dir = format!("/{}", project_dir);
            }
            println!("cd {}", project_dir);
            println!("{} --resume {}", claude_cmd(), session_id);
        }
        Err(_) => {
            eprintln!("Message not found: {}", uuid);
            std::process::exit(1);
        }
    }

    Ok(())
}
