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
