//! Accounts and sessions.
//!
//! These live in their own database, *not* in `library.db`: indexing rebuilds
//! the library from scratch every run, which would wipe any accounts stored
//! there. Everything durable and user-owned belongs here: the cook log,
//! favourites, tags and notes live in `marks`, on the same connection.
//!
//! This one is plain SQLite rather than Turso. Turso takes an exclusive lock
//! on the file for the lifetime of the connection, so `user add` would fail
//! while the server is running - and adding users while it serves is the whole
//! point of the CLI. SQLite in WAL mode handles the two processes fine.

use anyhow::{Context, Result, anyhow, bail};
use argon2::Argon2;
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rusqlite::{Connection, OptionalExtension, params};

pub const DEFAULT_AUTH_DB: &str = "auth.db";
pub const COOKIE: &str = "cookbook_session";
/// Sessions last a month; this is a home library, not a bank.
const SESSION_DAYS: i64 = 30;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    token      TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
"#;

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub username: String,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {path}"))?;
    // WAL plus a busy timeout is what lets the CLI write while the server reads.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;
    // Per-user recipe marks live in the same database, for the same reason.
    conn.execute_batch(crate::marks::SCHEMA)?;
    Ok(conn)
}

// ------------------------------------------------------------------ passwords

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("hashing password: {e}"))?
        .to_string())
}

fn verify_password(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

fn random_token() -> String {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------- users

fn find_user(conn: &Connection, username: &str) -> Result<Option<(i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT id, password_hash FROM users WHERE username = ?1",
            params![username],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub fn add_user(conn: &Connection, username: &str, password: &str) -> Result<()> {
    let username = username.trim();
    if username.is_empty() {
        bail!("username must not be empty");
    }
    if password.is_empty() {
        bail!("password must not be empty");
    }
    if find_user(conn, username)?.is_some() {
        bail!("user {username} already exists");
    }
    conn.execute(
        "INSERT INTO users (username, password_hash, created_at) VALUES (?1, ?2, ?3)",
        params![username, hash_password(password)?, now()],
    )
    .context("inserting user")?;
    Ok(())
}

pub fn set_password(conn: &Connection, username: &str, password: &str) -> Result<()> {
    if password.is_empty() {
        bail!("password must not be empty");
    }
    let Some((id, _)) = find_user(conn, username)? else {
        bail!("no such user: {username}");
    };
    conn.execute(
        "UPDATE users SET password_hash = ?1 WHERE id = ?2",
        params![hash_password(password)?, id],
    )?;
    // Changing a password should end existing sessions.
    conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![id])?;
    Ok(())
}

pub fn remove_user(conn: &Connection, username: &str) -> Result<()> {
    let Some((id, _)) = find_user(conn, username)? else {
        bail!("no such user: {username}");
    };
    conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![id])?;
    // Their marks go too, or they sit orphaned forever - and a later account
    // reusing the id would inherit them.
    for table in ["recipe_marks", "cook_log", "recipe_tags"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE user_id = ?1"),
            params![id],
        )?;
    }
    conn.execute("DELETE FROM users WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn list_users(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare("SELECT username, created_at FROM users ORDER BY username")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn count_users(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
}

// ------------------------------------------------------------------- sessions

/// Verify credentials and open a session. Returns the session token.
pub fn login(conn: &Connection, username: &str, password: &str) -> Result<Option<String>> {
    let found = find_user(conn, username.trim())?;
    let Some((user_id, stored)) = found else {
        // Hash anyway so a missing user doesn't answer faster than a wrong
        // password and leak which usernames exist.
        let _ = hash_password(password);
        return Ok(None);
    };
    if !verify_password(password, &stored) {
        return Ok(None);
    }

    let token = random_token();
    let now = now();
    conn.execute(
        "INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![token, user_id, now, now + SESSION_DAYS * 24 * 60 * 60],
    )?;
    Ok(Some(token))
}

pub fn session_user(conn: &Connection, token: &str) -> Result<Option<User>> {
    if token.is_empty() {
        return Ok(None);
    }
    let found: Option<(i64, String, i64)> = conn
        .query_row(
            "SELECT u.id, u.username, s.expires_at FROM sessions s \
             JOIN users u ON u.id = s.user_id WHERE s.token = ?1",
            params![token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, username, expires)) = found else {
        return Ok(None);
    };
    if expires <= now() {
        let _ = logout(conn, token);
        return Ok(None);
    }
    Ok(Some(User { id, username }))
}

pub fn logout(conn: &Connection, token: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
    Ok(())
}

// -------------------------------------------------------------------- cookies

/// Pull our session token out of a `Cookie` header.
pub fn token_from_cookies(header: Option<&str>) -> Option<String> {
    header?.split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k.trim() == COOKIE).then(|| v.trim().to_string())
    })
}

pub fn set_cookie(token: &str, secure: bool) -> String {
    let max_age = SESSION_DAYS * 24 * 60 * 60;
    let s = if secure { "; Secure" } else { "" };
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{s}")
}

pub fn clear_cookie(secure: bool) -> String {
    let s = if secure { "; Secure" } else { "" };
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{s}")
}
