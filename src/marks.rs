//! Per-user recipe marks: cook log, favourites, tags and comments.
//!
//! These live in `auth.db` alongside the accounts, never in `library.db` —
//! indexing rebuilds the library from scratch and would wipe them.
//!
//! Rows are keyed by a **stable recipe key**, not by `recipes.id`: the indexer
//! numbers recipes sequentially, so ids are reassigned on every re-index and
//! anything stored against them would silently point at the wrong recipe
//! afterwards. The key is `filename|doc|anchor`, all of which come from the
//! epub itself and survive a rebuild. `server::recipe_key` builds it.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS recipe_marks (
    user_id    INTEGER NOT NULL,
    recipe_key TEXT NOT NULL,
    favorite   INTEGER NOT NULL DEFAULT 0,
    comment    TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, recipe_key)
);

CREATE TABLE IF NOT EXISTS cook_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL,
    recipe_key TEXT NOT NULL,
    cooked_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS cook_log_user ON cook_log (user_id, cooked_at DESC);

CREATE TABLE IF NOT EXISTS recipe_tags (
    user_id    INTEGER NOT NULL,
    recipe_key TEXT NOT NULL,
    tag        TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, recipe_key, tag)
);
CREATE INDEX IF NOT EXISTS recipe_tags_tag ON recipe_tags (user_id, tag);
"#;

/// One entry in the cook log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Cook {
    pub id: i64,
    pub at: i64,
}

/// Everything one user has recorded about one recipe.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Marks {
    pub favorite: bool,
    pub comment: String,
    pub tags: Vec<String>,
    pub cooked: Vec<Cook>,
}

/// Tags are matched case-insensitively, so store them folded.
pub fn normalize_tag(tag: &str) -> String {
    tag.trim().to_lowercase()
}

pub fn get(conn: &Connection, user_id: i64, key: &str) -> Result<Marks> {
    let mut m = Marks::default();

    if let Some((fav, comment)) = conn
        .query_row(
            "SELECT favorite, comment FROM recipe_marks WHERE user_id = ?1 AND recipe_key = ?2",
            params![user_id, key],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        m.favorite = fav != 0;
        m.comment = comment;
    }

    let mut stmt = conn.prepare(
        "SELECT tag FROM recipe_tags WHERE user_id = ?1 AND recipe_key = ?2 ORDER BY tag",
    )?;
    m.tags = stmt
        .query_map(params![user_id, key], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT id, cooked_at FROM cook_log WHERE user_id = ?1 AND recipe_key = ?2 \
         ORDER BY cooked_at DESC",
    )?;
    m.cooked = stmt
        .query_map(params![user_id, key], |r| {
            Ok(Cook {
                id: r.get(0)?,
                at: r.get(1)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(m)
}

fn touch(conn: &Connection, user_id: i64, key: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO recipe_marks (user_id, recipe_key, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT (user_id, recipe_key) DO NOTHING",
        params![user_id, key, crate::auth::now()],
    )?;
    Ok(())
}

/// Drop a row that no longer says anything, so unfavouriting or clearing a
/// note leaves no trace behind. Tags and the cook log are separate tables.
fn prune(conn: &Connection, user_id: i64, key: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM recipe_marks WHERE user_id = ?1 AND recipe_key = ?2 \
         AND favorite = 0 AND comment = ''",
        params![user_id, key],
    )?;
    Ok(())
}

pub fn set_favorite(conn: &Connection, user_id: i64, key: &str, on: bool) -> Result<()> {
    touch(conn, user_id, key)?;
    conn.execute(
        "UPDATE recipe_marks SET favorite = ?1, updated_at = ?2 \
         WHERE user_id = ?3 AND recipe_key = ?4",
        params![on as i64, crate::auth::now(), user_id, key],
    )?;
    prune(conn, user_id, key)
}

pub fn set_comment(conn: &Connection, user_id: i64, key: &str, text: &str) -> Result<()> {
    touch(conn, user_id, key)?;
    conn.execute(
        "UPDATE recipe_marks SET comment = ?1, updated_at = ?2 \
         WHERE user_id = ?3 AND recipe_key = ?4",
        params![text.trim(), crate::auth::now(), user_id, key],
    )?;
    prune(conn, user_id, key)
}

/// Record a cooking session. `at` is a unix timestamp so the caller can
/// backdate an entry.
pub fn add_cook(conn: &Connection, user_id: i64, key: &str, at: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO cook_log (user_id, recipe_key, cooked_at) VALUES (?1, ?2, ?3)",
        params![user_id, key, at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Delete one cook-log entry. Scoped by user so an id from another account
/// cannot be removed.
pub fn remove_cook(conn: &Connection, user_id: i64, entry_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM cook_log WHERE id = ?1 AND user_id = ?2",
        params![entry_id, user_id],
    )?;
    Ok(())
}

pub fn add_tag(conn: &Connection, user_id: i64, key: &str, tag: &str) -> Result<()> {
    let tag = normalize_tag(tag);
    if tag.is_empty() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO recipe_tags (user_id, recipe_key, tag, created_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (user_id, recipe_key, tag) DO NOTHING",
        params![user_id, key, tag, crate::auth::now()],
    )?;
    Ok(())
}

pub fn remove_tag(conn: &Connection, user_id: i64, key: &str, tag: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM recipe_tags WHERE user_id = ?1 AND recipe_key = ?2 AND tag = ?3",
        params![user_id, key, normalize_tag(tag)],
    )?;
    Ok(())
}

// ------------------------------------------------------------------- listings

/// Most recent cook first. Returns (entry id, recipe key, timestamp).
pub fn history(conn: &Connection, user_id: i64, limit: i64, offset: i64) -> Result<Vec<Cook3>> {
    let mut stmt = conn.prepare(
        "SELECT id, recipe_key, cooked_at FROM cook_log WHERE user_id = ?1 \
         ORDER BY cooked_at DESC, id DESC LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![user_id, limit, offset], |r| {
        Ok(Cook3 {
            id: r.get(0)?,
            key: r.get(1)?,
            at: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub struct Cook3 {
    pub id: i64,
    pub key: String,
    pub at: i64,
}

/// Favourited recipes, most recently favourited first.
pub fn favorites(conn: &Connection, user_id: i64, limit: i64, offset: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT recipe_key FROM recipe_marks WHERE user_id = ?1 AND favorite = 1 \
         ORDER BY updated_at DESC LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![user_id, limit, offset], |r| r.get(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Every tag the user has used, with how many recipes carry it.
pub fn tag_counts(conn: &Connection, user_id: i64) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT tag, COUNT(*) FROM recipe_tags WHERE user_id = ?1 \
         GROUP BY tag ORDER BY COUNT(*) DESC, tag",
    )?;
    let rows = stmt.query_map(params![user_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn tagged(
    conn: &Connection,
    user_id: i64,
    tag: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT recipe_key FROM recipe_tags WHERE user_id = ?1 AND tag = ?2 \
         ORDER BY created_at DESC LIMIT ?3 OFFSET ?4",
    )?;
    let rows = stmt.query_map(params![user_id, normalize_tag(tag), limit, offset], |r| {
        r.get(0)
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Key sets used to badge search results. Both are small enough (a user's own
/// marks) to load per request.
pub fn favorite_keys(conn: &Connection, user_id: i64) -> Result<HashSet<String>> {
    let mut stmt =
        conn.prepare("SELECT recipe_key FROM recipe_marks WHERE user_id = ?1 AND favorite = 1")?;
    let rows = stmt.query_map(params![user_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<HashSet<_>, _>>()?)
}

pub fn cooked_keys(conn: &Connection, user_id: i64) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT recipe_key FROM cook_log WHERE user_id = ?1")?;
    let rows = stmt.query_map(params![user_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<HashSet<_>, _>>()?)
}
