mod db;
mod epub;
mod extract;
mod indexer;
mod server;

use anyhow::{Result, bail};
use std::path::PathBuf;

const DEFAULT_DB: &str = "library.db";

fn usage() -> ! {
    eprintln!(
        "cooking-ebook-viewer

USAGE:
  cooking-ebook-viewer index --library <dir> [--db <file>]
  cooking-ebook-viewer serve [--db <file>] [--addr <host:port>]
  cooking-ebook-viewer stats [--db <file>]

DEFAULTS:
  --db       {DEFAULT_DB}
  --addr     127.0.0.1:8080"
    );
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }

    let opt_flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let flag = |name: &str, default: &str| -> String {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| default.to_string())
    };

    match args[0].as_str() {
        "index" => {
            let Some(library) = opt_flag("--library") else {
                eprintln!("index requires --library <dir>\n");
                usage()
            };
            let library = PathBuf::from(library);
            if !library.is_dir() {
                bail!("library directory not found: {}", library.display());
            }
            indexer::run(&library, &flag("--db", DEFAULT_DB)).await
        }
        "serve" => {
            let db_path = flag("--db", DEFAULT_DB);
            if !std::path::Path::new(&db_path).exists() {
                bail!("database {db_path} not found - run `index` first");
            }
            server::run(&db_path, &flag("--addr", "127.0.0.1:8080")).await
        }
        "stats" => indexer::stats(&flag("--db", DEFAULT_DB)).await,
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("unknown command: {other}\n");
            usage()
        }
    }
}
