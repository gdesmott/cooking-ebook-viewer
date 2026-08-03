//! Turso schema and queries.
//!
//! Note: Turso has no FTS5. Full-text search uses its Tantivy-backed index
//! (`CREATE INDEX ... USING fts`) and the `fts_match` / `fts_score` /
//! `fts_highlight` functions.

use anyhow::Result;
use turso::{Builder, Connection, Value};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS books (
    id        INTEGER PRIMARY KEY,
    filename  TEXT NOT NULL,
    path      TEXT NOT NULL,
    title     TEXT NOT NULL,
    author    TEXT,
    language  TEXT,
    cover     TEXT,
    n_recipes INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS recipes (
    id          INTEGER PRIMARY KEY,
    book_id     INTEGER NOT NULL,
    title       TEXT NOT NULL,
    chapter     TEXT,
    doc         TEXT NOT NULL,
    anchor      TEXT,
    position    INTEGER NOT NULL,
    image       TEXT,
    yield_text  TEXT,
    time_text   TEXT,
    ingredients TEXT NOT NULL DEFAULT '',
    body        TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS ingredients (
    id        INTEGER PRIMARY KEY,
    recipe_id INTEGER NOT NULL,
    book_id   INTEGER NOT NULL,
    line      TEXT NOT NULL
);
"#;

/// Built after bulk loading, so indexing happens once rather than per row.
pub const FTS_INDEX: &str = "CREATE INDEX IF NOT EXISTS recipes_fts ON recipes \
     USING fts (title, ingredients, body) \
     WITH (weights = 'title=3.0,ingredients=2.0,body=1.0')";

pub async fn open(path: &str) -> Result<Connection> {
    // `CREATE INDEX ... USING fts` is a custom index method, which Turso still
    // gates behind an experimental flag.
    let db = Builder::new_local(path)
        .experimental_index_method(true)
        .build()
        .await?;
    let conn = db.connect()?;
    Ok(conn)
}

pub async fn init(conn: &Connection) -> Result<()> {
    for stmt in SCHEMA.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            conn.execute(s, ()).await?;
        }
    }
    Ok(())
}

pub fn val_str(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    }
}

pub fn val_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Real(f) => Some(*f as i64),
        _ => None,
    }
}
