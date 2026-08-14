mod auth;
mod db;
mod epub;
mod extract;
mod indexer;
mod server;

use crate::auth::DEFAULT_AUTH_DB;
use anyhow::{Result, bail};
use std::path::PathBuf;

const DEFAULT_DB: &str = "library.db";

fn usage() -> ! {
    eprintln!(
        "cooking-ebook-viewer

USAGE:
  cooking-ebook-viewer index --library <dir> [--db <file>]
  cooking-ebook-viewer serve [--db <file>] [--auth-db <file>] [--addr <host:port>]
                             [--secure-cookie]
  cooking-ebook-viewer stats [--db <file>]

  cooking-ebook-viewer user add <name> [--auth-db <file>]
  cooking-ebook-viewer user passwd <name> [--auth-db <file>]
  cooking-ebook-viewer user rm <name> [--auth-db <file>]
  cooking-ebook-viewer user list [--auth-db <file>]

DEFAULTS:
  --db       {DEFAULT_DB}
  --auth-db  {DEFAULT_AUTH_DB}
  --addr     127.0.0.1:8080

Pass --secure-cookie when serving behind HTTPS."
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
            server::run(
                &db_path,
                &flag("--auth-db", DEFAULT_AUTH_DB),
                &flag("--addr", "127.0.0.1:8080"),
                args.iter().any(|a| a == "--secure-cookie"),
            )
            .await
        }
        "stats" => indexer::stats(&flag("--db", DEFAULT_DB)).await,
        "user" => {
            let conn = auth::open(&flag("--auth-db", DEFAULT_AUTH_DB))?;
            let sub = args.get(1).map(String::as_str).unwrap_or("");
            let name = args.get(2).cloned().unwrap_or_default();
            match sub {
                "list" => {
                    let users = auth::list_users(&conn)?;
                    if users.is_empty() {
                        println!("no users yet - add one with `user add <name>`");
                    }
                    for (u, _) in users {
                        println!("{u}");
                    }
                    Ok(())
                }
                "add" | "passwd" | "rm" if name.is_empty() => {
                    eprintln!("user {sub} requires a username\n");
                    usage()
                }
                "add" => {
                    let pw = prompt_new_password()?;
                    auth::add_user(&conn, &name, &pw)?;
                    println!("added user {name}");
                    Ok(())
                }
                "passwd" => {
                    let pw = prompt_new_password()?;
                    auth::set_password(&conn, &name, &pw)?;
                    println!("password updated for {name}; existing sessions signed out");
                    Ok(())
                }
                "rm" => {
                    auth::remove_user(&conn, &name)?;
                    println!("removed user {name}");
                    Ok(())
                }
                other => {
                    eprintln!("unknown user command: {other}\n");
                    usage()
                }
            }
        }
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("unknown command: {other}\n");
            usage()
        }
    }
}

/// Interactively: ask twice, without echoing, so a typo can't lock the account
/// out. When stdin is piped (scripts, `echo pw | ... user add bob`), take a
/// single line instead — rpassword reads /dev/tty and would fail outright.
fn prompt_new_password() -> Result<String> {
    use std::io::{BufRead, IsTerminal};

    let pw = if std::io::stdin().is_terminal() {
        let pw = rpassword::prompt_password("New password: ")?;
        if rpassword::prompt_password("Repeat password: ")? != pw {
            bail!("passwords do not match");
        }
        pw
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\n', '\r']).to_string()
    };

    if pw.is_empty() {
        bail!("password must not be empty");
    }
    Ok(pw)
}
