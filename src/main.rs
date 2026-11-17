mod bigram;
mod cli;
mod date_utils;
mod db;
mod error;
mod git_utils;
mod indexer;
mod schema;
mod search;
mod summarization;

use clap::Parser;

fn main() {
    env_logger::init();

    let cli = cli::Cli::parse();
    if let Err(e) = cli::run(cli) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parses(args: &[&str]) -> bool {
        cli::Cli::try_parse_from(
            std::iter::once("ai-conversation-search").chain(args.iter().copied()),
        )
        .is_ok()
    }

    #[test]
    fn test_days_is_limited_to_what_index_all_uses() {
        // `index --all` hands 9999 to the OpenCode/Codex indexers, so that is the
        // largest window any command needs. Larger values overflowed date math.
        for sub in [&["init"][..], &["index"], &["search", "q"], &["list"]] {
            for (days, ok) in [("0", true), ("9999", true), ("10000", false), ("-1", false)] {
                let args: Vec<&str> = sub.iter().copied().chain(["--days", days]).collect();
                assert_eq!(parses(&args), ok, "{args:?}");
            }
        }
    }
}
