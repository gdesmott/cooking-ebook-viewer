//! Axum HTTP server. Book pages and images are streamed straight out of the
//! epub zips on demand, so nothing is unpacked to disk.

use crate::auth;
use crate::db;
use crate::epub::{Epub, resolve};
use crate::marks;
use anyhow::Result;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
    routing::{delete, get, post},
};
use serde::Serialize;
use serde_json::{Value as J, json};
use std::collections::HashMap;
use std::sync::Arc;
use turso::{Database, Value, params};

struct AppState {
    db: Database,
    /// Accounts and sessions; a separate SQLite database from the library,
    /// which is rebuilt from scratch on every index.
    auth: std::sync::Mutex<rusqlite::Connection>,
    secure_cookie: bool,
    /// book id -> epub path on disk, resolved once at startup.
    books: HashMap<i64, std::path::PathBuf>,
    /// recipe id -> stable key, and back. User marks are stored against the
    /// key, because ids are reassigned by every re-index. Resolved once at
    /// startup: Turso holds the library file locked for the life of the
    /// connection, so it cannot change underneath a running server.
    keys: HashMap<i64, String>,
    ids: HashMap<String, i64>,
}

/// The identity of a recipe that survives a re-index: which epub, which
/// document inside it, and which anchor within that document. See `marks`.
fn recipe_key(filename: &str, doc: &str, anchor: Option<&str>) -> String {
    format!("{filename}|{doc}|{}", anchor.unwrap_or(""))
}

type St = Arc<AppState>;

pub async fn run(db_path: &str, auth_db: &str, addr: &str, secure_cookie: bool) -> Result<()> {
    let conn = db::open(db_path).await?;

    let mut books = HashMap::new();
    let mut rows = conn.query("SELECT id, path FROM books", ()).await?;
    while let Some(row) = rows.next().await? {
        let id = db::val_i64(&row.get_value(0)?).unwrap_or_default();
        if let Some(p) = db::val_str(&row.get_value(1)?) {
            books.insert(id, std::path::PathBuf::from(p));
        }
    }
    let n_books = books.len();
    drop(rows);

    let mut keys = HashMap::new();
    let mut ids = HashMap::new();
    let mut rows = conn
        .query(
            "SELECT r.id, b.filename, r.doc, r.anchor FROM recipes r \
             JOIN books b ON b.id = r.book_id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let id = db::val_i64(&row.get_value(0)?).unwrap_or_default();
        let key = recipe_key(
            &db::val_str(&row.get_value(1)?).unwrap_or_default(),
            &db::val_str(&row.get_value(2)?).unwrap_or_default(),
            db::val_str(&row.get_value(3)?).as_deref(),
        );
        ids.insert(key.clone(), id);
        keys.insert(id, key);
    }
    drop(rows);
    // Two recipes sharing a key would share each other's marks. That means a
    // book put several TOC entries in one document without anchors to tell
    // them apart; say so rather than silently merging them.
    if ids.len() < keys.len() {
        println!(
            "WARNING: {} recipes share a stable key with another and will share marks",
            keys.len() - ids.len()
        );
    }

    let database = turso::Builder::new_local(db_path)
        .experimental_index_method(true)
        .build()
        .await?;

    let auth_conn = auth::open(auth_db)?;
    let n_users = auth::count_users(&auth_conn)?;

    let state: St = Arc::new(AppState {
        db: database,
        auth: std::sync::Mutex::new(auth_conn),
        secure_cookie,
        books,
        keys,
        ids,
    });

    // The UI and API carry no validators, so browsers heuristically cache them
    // and keep serving results from before a re-index. Book assets are
    // immutable for a given path and stay cacheable.
    let dynamic = Router::new()
        .route("/", get(index_html))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/api/me", get(api_me))
        .route("/api/books", get(api_books))
        .route("/api/books/{id}/toc", get(api_book_toc))
        .route("/api/search", get(api_search))
        .route("/api/recipes/{id}", get(api_recipe))
        .route("/api/ingredients", get(api_ingredients))
        // Per-user marks: cooked, favourite, tags, comment.
        .route("/api/recipes/{id}/cook", post(api_cook))
        .route("/api/recipes/{id}/cook/{entry}", delete(api_uncook))
        .route("/api/recipes/{id}/favorite", post(api_favorite))
        .route("/api/recipes/{id}/comment", post(api_comment))
        .route("/api/recipes/{id}/tags", post(api_add_tag))
        .route("/api/recipes/{id}/tags/{tag}", delete(api_remove_tag))
        .route("/api/history", get(api_history))
        .route("/api/favorites", get(api_favorites))
        .route("/api/favorites/all", get(api_favorites_all))
        .route("/api/tags", get(api_tags))
        .route("/api/tags/{tag}", get(api_tagged))
        .layer(axum::middleware::from_fn(no_store));

    let assets = Router::new()
        .route("/cover/{book_id}", get(cover))
        .route("/page/{book_id}/{*path}", get(page))
        .route("/asset/{book_id}/{*path}", get(asset));

    // Everything above requires a session. Only the login form is public.
    let protected = dynamic
        .merge(assets)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    let public = Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", axum::routing::post(logout))
        .layer(axum::middleware::from_fn(no_store));

    let app = protected.merge(public).with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Serving {n_books} books on http://{addr}");
    if n_users == 0 {
        println!(
            "WARNING: no accounts exist, so nobody can sign in.\n\
             Create one with: cooking-ebook-viewer user add <name>"
        );
    }
    axum::serve(listener, app).await?;
    Ok(())
}

// ----------------------------------------------------------------------- auth

/// Gate every content route on a valid session. API routes get a 401 so the
/// frontend can react; page requests are redirected to the login form.
async fn require_auth(
    State(st): State<St>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let token = auth::token_from_cookies(
        req.headers()
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    )
    .unwrap_or_default();

    // Scoped so the guard is dropped before the await below.
    let user = {
        let conn = st.auth.lock().expect("auth db mutex");
        auth::session_user(&conn, &token).ok().flatten()
    };

    let Some(user) = user else {
        let path = req.uri().path();
        if path.starts_with("/api/") {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "unauthorized" })),
            )
                .into_response();
        }
        let next_to = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let location = format!("/login?next={}", urlencode(next_to));
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, location)],
            "Sign in required",
        )
            .into_response();
    };

    let mut req = req;
    req.extensions_mut().insert(user);
    next.run(req).await
}

fn urlencode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Only ever redirect within this site.
fn safe_next(next: &str) -> String {
    if next.starts_with('/') && !next.starts_with("//") {
        next.to_string()
    } else {
        "/".to_string()
    }
}

fn render_login(next: &str, error: Option<&str>) -> Html<String> {
    let page = include_str!("../static/login.html")
        .replace("<!--NEXT-->", &html_escape(&safe_next(next)))
        .replace(
            "<!--ERROR-->",
            &error
                .map(|e| format!("<p class=\"error\">{}</p>", html_escape(e)))
                .unwrap_or_default(),
        );
    Html(page)
}

#[derive(serde::Deserialize)]
struct NextQ {
    next: Option<String>,
}

async fn login_page(Query(q): Query<NextQ>) -> Response {
    render_login(&q.next.unwrap_or_else(|| "/".into()), None).into_response()
}

#[derive(serde::Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    next: Option<String>,
}

async fn login_submit(
    State(st): State<St>,
    axum::extract::Form(f): axum::extract::Form<LoginForm>,
) -> Response {
    let next = safe_next(f.next.as_deref().unwrap_or("/"));

    let result = {
        let conn = st.auth.lock().expect("auth db mutex");
        auth::login(&conn, &f.username, &f.password)
    };

    match result {
        Ok(Some(token)) => (
            StatusCode::SEE_OTHER,
            [
                (header::LOCATION, next),
                (
                    header::SET_COOKIE,
                    auth::set_cookie(&token, st.secure_cookie),
                ),
            ],
            "Signed in",
        )
            .into_response(),
        Ok(None) => (
            StatusCode::UNAUTHORIZED,
            render_login(&next, Some("Incorrect username or password.")),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn logout(State(st): State<St>, headers: HeaderMap) -> Response {
    let token = auth::token_from_cookies(headers.get(header::COOKIE).and_then(|v| v.to_str().ok()))
        .unwrap_or_default();
    {
        let conn = st.auth.lock().expect("auth db mutex");
        let _ = auth::logout(&conn, &token);
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/login".to_string()),
            (header::SET_COOKIE, auth::clear_cookie(st.secure_cookie)),
        ],
        "Signed out",
    )
        .into_response()
}

/// Who am I — lets the UI show the signed-in user.
async fn api_me(axum::Extension(user): axum::Extension<auth::User>) -> Json<J> {
    Json(json!({ "id": user.id, "username": user.username }))
}

async fn no_store(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, must-revalidate"),
    );
    res
}

// ---------------------------------------------------------------- static UI

async fn index_html() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn app_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../static/app.js"),
    )
}

async fn app_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/app.css"),
    )
}

// -------------------------------------------------------------------- error

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

// ---------------------------------------------------------------------- API

#[derive(Serialize)]
struct BookOut {
    id: i64,
    title: String,
    author: Option<String>,
    n_recipes: i64,
}

async fn api_books(State(st): State<St>) -> Result<Json<Vec<BookOut>>, AppError> {
    let conn = st.db.connect()?;
    let mut rows = conn
        .query(
            "SELECT id, title, author, n_recipes FROM books ORDER BY title",
            (),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        out.push(BookOut {
            id: db::val_i64(&r.get_value(0)?).unwrap_or_default(),
            title: db::val_str(&r.get_value(1)?).unwrap_or_default(),
            author: db::val_str(&r.get_value(2)?),
            n_recipes: db::val_i64(&r.get_value(3)?).unwrap_or_default(),
        });
    }
    Ok(Json(out))
}

/// Chapter -> recipes, for the book reader sidebar.
async fn api_book_toc(State(st): State<St>, Path(id): Path<i64>) -> Result<Json<J>, AppError> {
    let conn = st.db.connect()?;

    let mut rows = conn
        .query("SELECT title, author FROM books WHERE id = ?1", params![id])
        .await?;
    let (title, author) = match rows.next().await? {
        Some(r) => (
            db::val_str(&r.get_value(0)?).unwrap_or_default(),
            db::val_str(&r.get_value(1)?),
        ),
        None => return Err(AppError(anyhow::anyhow!("no such book"))),
    };

    let mut rows = conn
        .query(
            "SELECT id, title, chapter, doc, anchor, image, yield_text, time_text \
             FROM recipes WHERE book_id = ?1 ORDER BY position",
            params![id],
        )
        .await?;
    let mut recipes = Vec::new();
    while let Some(r) = rows.next().await? {
        recipes.push(json!({
            "id": db::val_i64(&r.get_value(0)?),
            "title": db::val_str(&r.get_value(1)?),
            "chapter": db::val_str(&r.get_value(2)?),
            "doc": db::val_str(&r.get_value(3)?),
            "anchor": db::val_str(&r.get_value(4)?),
            "image": db::val_str(&r.get_value(5)?),
            "yield": db::val_str(&r.get_value(6)?),
            "time": db::val_str(&r.get_value(7)?),
        }));
    }

    Ok(Json(
        json!({ "id": id, "title": title, "author": author, "recipes": recipes }),
    ))
}

#[derive(serde::Deserialize)]
struct SearchQ {
    q: Option<String>,
    book: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
    sort: Option<String>,
}

async fn api_search(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Query(qp): Query<SearchQ>,
) -> Result<Json<J>, AppError> {
    let conn = st.db.connect()?;
    let limit = qp.limit.unwrap_or(60).clamp(1, 300);
    let offset = qp.offset.unwrap_or(0).max(0);
    let q = qp.q.clone().unwrap_or_default().trim().to_string();

    let mut items = Vec::new();

    if q.is_empty() {
        // Browsing the global index: no query, just ordered listing.
        let mut prm = vec![Value::Integer(limit), Value::Integer(offset)];
        let sql = match qp.book {
            Some(b) => {
                prm.push(Value::Integer(b));
                "SELECT r.id, r.title, r.chapter, r.book_id, b.title, r.image, \
                 r.yield_text, r.time_text, '' \
                 FROM recipes r JOIN books b ON b.id = r.book_id \
                 WHERE r.ingredients != '' AND r.book_id = ?3 \
                 ORDER BY r.title LIMIT ?1 OFFSET ?2"
            }
            None => {
                "SELECT r.id, r.title, r.chapter, r.book_id, b.title, r.image, \
                 r.yield_text, r.time_text, '' \
                 FROM recipes r JOIN books b ON b.id = r.book_id \
                 WHERE r.ingredients != '' ORDER BY r.title LIMIT ?1 OFFSET ?2"
            }
        };
        let mut rows = conn.query(sql, prm).await?;
        while let Some(r) = rows.next().await? {
            items.push(hit_json(&st, &r)?);
        }
    } else {
        // Turso FTS: fts_match filters, fts_score ranks (BM25), fts_highlight
        // marks the matched terms in the ingredient list.
        let base = "SELECT r.id, r.title, r.chapter, r.book_id, b.title, r.image, \
             r.yield_text, r.time_text, \
             fts_highlight(r.title, r.ingredients, r.body, '<mark>', '</mark>', ?4) \
             FROM recipes r JOIN books b ON b.id = r.book_id \
             WHERE fts_match(r.title, r.ingredients, r.body, ?3) \
             AND r.ingredients != ''";
        let order = if qp.sort.as_deref() == Some("title") {
            "ORDER BY r.title"
        } else {
            "ORDER BY fts_score(r.title, r.ingredients, r.body, ?3) DESC"
        };

        let mut prm = vec![
            Value::Integer(limit),
            Value::Integer(offset),
            Value::Text(build_fts_query(&q)),
            Value::Text(build_highlight_query(&q)),
        ];
        let sql = match qp.book {
            Some(b) => {
                prm.push(Value::Integer(b));
                format!("{base} AND r.book_id = ?5 {order} LIMIT ?1 OFFSET ?2")
            }
            None => format!("{base} {order} LIMIT ?1 OFFSET ?2"),
        };
        let mut rows = conn.query(&sql, prm).await?;
        while let Some(r) = rows.next().await? {
            items.push(hit_json(&st, &r)?);
        }
    }

    badge(&st, user.id, &mut items)?;
    Ok(Json(json!({ "items": items, "q": q, "offset": offset })))
}

/// Flag which of these cards the user has favourited or already cooked. Both
/// sets are one user's own marks, small enough to load per request.
fn badge(st: &St, user_id: i64, items: &mut [J]) -> Result<(), AppError> {
    if items.is_empty() {
        return Ok(());
    }
    let (favs, cooked) = {
        let conn = st.auth.lock().expect("auth db mutex");
        (
            marks::favorite_keys(&conn, user_id)?,
            marks::cooked_keys(&conn, user_id)?,
        )
    };
    for it in items {
        let key = it
            .get("key")
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string();
        it["fav"] = json!(favs.contains(&key));
        it["cooked"] = json!(cooked.contains(&key));
    }
    Ok(())
}

/// Tantivy's query parser ORs bare terms, so "air fryer" matches any recipe
/// merely containing "air". Require every term instead, which is what someone
/// searching two ingredients means. Explicit query syntax is passed through so
/// AND/OR/NOT, "phrases", prefix* and field:term all still work.
fn build_fts_query(input: &str) -> String {
    let q = input.trim();
    let terms: Vec<&str> = q.split_whitespace().collect();
    let explicit = q.contains(['"', ':', '*', '^', '(', ')'])
        || terms
            .iter()
            .any(|t| matches!(*t, "AND" | "OR" | "NOT") || t.starts_with(['+', '-']));
    if explicit || terms.len() < 2 {
        return q.to_string();
    }
    terms.join(" AND ")
}

/// Terms only, for `fts_highlight`. It marks whatever it is given, so handing
/// it the match query would highlight the literal "and" in "Salt and pepper"
/// and drag the snippet window onto an irrelevant part of the recipe.
fn build_highlight_query(input: &str) -> String {
    let terms: Vec<String> = input
        .split_whitespace()
        .filter(|t| !matches!(*t, "AND" | "OR" | "NOT"))
        .map(|t| {
            let t = t.trim_start_matches(['+', '-']);
            // Drop a field prefix ("title:harissa") and any phrase quoting.
            let t = t.split_once(':').map(|(_, r)| r).unwrap_or(t);
            t.trim_matches('"').to_string()
        })
        .filter(|t| !t.is_empty())
        .collect();
    terms.join(" ")
}

/// Trim a highlighted field down to a window around the first match, so cards
/// show the relevant ingredient rather than the whole list.
fn make_snippet(s: Option<String>) -> Option<String> {
    let s = s?;
    let flat = s.replace('\n', " · ");
    if flat.trim().is_empty() {
        return None;
    }
    let Some(hit) = flat.find("<mark>") else {
        return Some(flat.chars().take(160).collect());
    };

    // Work in chars throughout: ingredient text is full of multi-byte
    // characters (·, ½, –) and byte offsets slice straight through them.
    let chars: Vec<char> = flat.chars().collect();
    let hit_char = flat[..hit].chars().count();

    // Start a little before the match, on a separator where possible.
    let mut start = hit_char.saturating_sub(60);
    if let Some(sep) = (start..hit_char).rev().find(|&i| chars[i] == '·') {
        start = (sep + 1).min(hit_char);
    }
    let end = (start + 190).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();

    // Never cut in the middle of a highlight tag.
    if let Some(open) = out.rfind("<mark>")
        && out[open..].find("</mark>").is_none()
    {
        out.truncate(open);
    }

    let prefix = if start > 0 { "… " } else { "" };
    let suffix = if end < chars.len() { " …" } else { "" };
    Some(format!("{prefix}{}{suffix}", out.trim()))
}

fn hit_json(st: &St, r: &turso::Row) -> Result<J, AppError> {
    let id = db::val_i64(&r.get_value(0)?).unwrap_or_default();
    Ok(json!({
        "id": id,
        "key": st.keys.get(&id),
        "title": db::val_str(&r.get_value(1)?),
        "chapter": db::val_str(&r.get_value(2)?),
        "book_id": db::val_i64(&r.get_value(3)?),
        "book": db::val_str(&r.get_value(4)?),
        "image": db::val_str(&r.get_value(5)?),
        "yield": db::val_str(&r.get_value(6)?),
        "time": db::val_str(&r.get_value(7)?),
        "snippet": make_snippet(db::val_str(&r.get_value(8)?)),
    }))
}

async fn api_recipe(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Path(id): Path<i64>,
) -> Result<Json<J>, AppError> {
    let conn = st.db.connect()?;
    let mut rows = conn
        .query(
            "SELECT r.id, r.title, r.chapter, r.book_id, b.title, r.doc, r.anchor, \
             r.image, r.yield_text, r.time_text, r.ingredients, r.body \
             FROM recipes r JOIN books b ON b.id = r.book_id WHERE r.id = ?1",
            params![id],
        )
        .await?;
    let Some(r) = rows.next().await? else {
        return Err(AppError(anyhow::anyhow!("no such recipe")));
    };
    let ingredients: Vec<String> = db::val_str(&r.get_value(10)?)
        .unwrap_or_default()
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let key = key_of(&st, id)?;
    let m = {
        let auth_conn = st.auth.lock().expect("auth db mutex");
        marks::get(&auth_conn, user.id, &key)?
    };
    Ok(Json(json!({
        "id": db::val_i64(&r.get_value(0)?),
        "key": key,
        "title": db::val_str(&r.get_value(1)?),
        "chapter": db::val_str(&r.get_value(2)?),
        "book_id": db::val_i64(&r.get_value(3)?),
        "book": db::val_str(&r.get_value(4)?),
        "doc": db::val_str(&r.get_value(5)?),
        "anchor": db::val_str(&r.get_value(6)?),
        "image": db::val_str(&r.get_value(7)?),
        "yield": db::val_str(&r.get_value(8)?),
        "time": db::val_str(&r.get_value(9)?),
        "ingredients": ingredients,
        "body": db::val_str(&r.get_value(11)?),
        "marks": m,
    })))
}

// -------------------------------------------------------------- user marks

/// The stable key for a recipe id, resolved from the map built at startup.
fn key_of(st: &St, id: i64) -> Result<String, AppError> {
    st.keys
        .get(&id)
        .cloned()
        .ok_or_else(|| AppError(anyhow::anyhow!("no such recipe")))
}

/// Run a closure against the accounts database and return the recipe's marks.
/// The guard is taken and dropped inside, so no `MutexGuard` is ever held
/// across an await - the future would stop being `Send`.
fn with_marks<F>(st: &St, user_id: i64, key: &str, f: F) -> Result<Json<J>, AppError>
where
    F: FnOnce(&rusqlite::Connection) -> Result<()>,
{
    let m = {
        let conn = st.auth.lock().expect("auth db mutex");
        f(&conn)?;
        marks::get(&conn, user_id, key)?
    };
    Ok(Json(json!(m)))
}

#[derive(serde::Deserialize)]
struct CookBody {
    /// Unix timestamp; defaults to now, so the UI can backdate an entry.
    at: Option<i64>,
}

async fn api_cook(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Path(id): Path<i64>,
    body: Option<Json<CookBody>>,
) -> Result<Json<J>, AppError> {
    let key = key_of(&st, id)?;
    let at = body.and_then(|Json(b)| b.at).unwrap_or_else(auth::now);
    with_marks(&st, user.id, &key, |c| {
        marks::add_cook(c, user.id, &key, at).map(|_| ())
    })
}

async fn api_uncook(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Path((id, entry)): Path<(i64, i64)>,
) -> Result<Json<J>, AppError> {
    let key = key_of(&st, id)?;
    with_marks(&st, user.id, &key, |c| {
        marks::remove_cook(c, user.id, entry)
    })
}

#[derive(serde::Deserialize)]
struct FavoriteBody {
    on: bool,
}

async fn api_favorite(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Path(id): Path<i64>,
    Json(b): Json<FavoriteBody>,
) -> Result<Json<J>, AppError> {
    let key = key_of(&st, id)?;
    with_marks(&st, user.id, &key, |c| {
        marks::set_favorite(c, user.id, &key, b.on)
    })
}

#[derive(serde::Deserialize)]
struct CommentBody {
    text: String,
}

async fn api_comment(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Path(id): Path<i64>,
    Json(b): Json<CommentBody>,
) -> Result<Json<J>, AppError> {
    let key = key_of(&st, id)?;
    with_marks(&st, user.id, &key, |c| {
        marks::set_comment(c, user.id, &key, &b.text)
    })
}

#[derive(serde::Deserialize)]
struct TagBody {
    tag: String,
}

async fn api_add_tag(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Path(id): Path<i64>,
    Json(b): Json<TagBody>,
) -> Result<Json<J>, AppError> {
    let key = key_of(&st, id)?;
    with_marks(&st, user.id, &key, |c| {
        marks::add_tag(c, user.id, &key, &b.tag)
    })
}

async fn api_remove_tag(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Path((id, tag)): Path<(i64, String)>,
) -> Result<Json<J>, AppError> {
    let key = key_of(&st, id)?;
    with_marks(&st, user.id, &key, |c| {
        marks::remove_tag(c, user.id, &key, &tag)
    })
}

/// Recipe cards for a list of stable keys, in the order given. Keys whose
/// recipe is gone - the epub was removed from the library, say - are skipped
/// rather than erroring: the mark outlives the book.
async fn cards_for_keys(st: &St, keys: &[String]) -> Result<Vec<J>, AppError> {
    let pairs: Vec<(i64, &String)> = keys
        .iter()
        .filter_map(|k| st.ids.get(k).map(|id| (*id, k)))
        .collect();
    if pairs.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = (1..=pairs.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT r.id, r.title, r.chapter, r.book_id, b.title, r.image, \
         r.yield_text, r.time_text, '' \
         FROM recipes r JOIN books b ON b.id = r.book_id WHERE r.id IN ({placeholders})"
    );
    let prm: Vec<Value> = pairs.iter().map(|(id, _)| Value::Integer(*id)).collect();

    let conn = st.db.connect()?;
    let mut rows = conn.query(&sql, prm).await?;
    let mut by_id: HashMap<i64, J> = HashMap::new();
    while let Some(r) = rows.next().await? {
        let card = hit_json(st, &r)?;
        if let Some(id) = card.get("id").and_then(|v| v.as_i64()) {
            by_id.insert(id, card);
        }
    }

    // `IN` gives no ordering guarantee, so restore the caller's order here.
    Ok(pairs
        .iter()
        .filter_map(|(id, _)| by_id.get(id).cloned())
        .collect())
}

#[derive(serde::Deserialize)]
struct PageQ {
    limit: Option<i64>,
    offset: Option<i64>,
}

impl PageQ {
    fn bounds(&self, default: i64) -> (i64, i64) {
        (
            self.limit.unwrap_or(default).clamp(1, 300),
            self.offset.unwrap_or(0).max(0),
        )
    }
}

/// The cook log: every session, newest first, with the recipe it was for.
async fn api_history(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Query(qp): Query<PageQ>,
) -> Result<Json<J>, AppError> {
    let (limit, offset) = qp.bounds(60);
    let entries = {
        let conn = st.auth.lock().expect("auth db mutex");
        marks::history(&conn, user.id, limit, offset)?
    };

    let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
    let cards = cards_for_keys(&st, &keys).await?;
    let by_key: HashMap<&str, &J> = cards
        .iter()
        .filter_map(|c| c.get("key").and_then(|k| k.as_str()).map(|k| (k, c)))
        .collect();

    // One item per cooking session: the same recipe appears once per time it
    // was cooked, which is the point of a history.
    let mut items = Vec::new();
    for e in &entries {
        let Some(card) = by_key.get(e.key.as_str()) else {
            continue;
        };
        let mut item = (*card).clone();
        item["entry_id"] = json!(e.id);
        item["cooked_at"] = json!(e.at);
        items.push(item);
    }
    badge(&st, user.id, &mut items)?;
    Ok(Json(json!({ "items": items, "offset": offset })))
}

async fn api_favorites(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Query(qp): Query<PageQ>,
) -> Result<Json<J>, AppError> {
    let (limit, offset) = qp.bounds(60);
    let keys = {
        let conn = st.auth.lock().expect("auth db mutex");
        marks::favorites(&conn, user.id, limit, offset)?
    };
    let mut items = cards_for_keys(&st, &keys).await?;
    badge(&st, user.id, &mut items)?;
    Ok(Json(json!({ "items": items, "offset": offset })))
}

/// Everyone's favourites, grouped by user. Not paginated - a household's
/// worth of favourites is small enough to send in one response.
async fn api_favorites_all(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
) -> Result<Json<J>, AppError> {
    let (usernames, all) = {
        let conn = st.auth.lock().expect("auth db mutex");
        (
            auth::list_users_with_id(&conn)?,
            marks::all_favorites(&conn)?,
        )
    };

    let mut keys_by_user: HashMap<i64, Vec<String>> = HashMap::new();
    for (uid, key) in all {
        keys_by_user.entry(uid).or_default().push(key);
    }

    let mut groups = Vec::new();
    for (uid, username) in usernames {
        let Some(keys) = keys_by_user.get(&uid) else {
            continue;
        };
        let mut items = cards_for_keys(&st, keys).await?;
        badge(&st, user.id, &mut items)?;
        groups.push(json!({ "username": username, "items": items }));
    }

    Ok(Json(json!({ "groups": groups })))
}

async fn api_tags(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
) -> Result<Json<J>, AppError> {
    let tags = {
        let conn = st.auth.lock().expect("auth db mutex");
        marks::tag_counts(&conn, user.id)?
    };
    let items: Vec<J> = tags
        .into_iter()
        .map(|(tag, n)| json!({ "tag": tag, "count": n }))
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn api_tagged(
    State(st): State<St>,
    axum::Extension(user): axum::Extension<auth::User>,
    Path(tag): Path<String>,
    Query(qp): Query<PageQ>,
) -> Result<Json<J>, AppError> {
    let (limit, offset) = qp.bounds(60);
    let keys = {
        let conn = st.auth.lock().expect("auth db mutex");
        marks::tagged(&conn, user.id, &tag, limit, offset)?
    };
    let mut items = cards_for_keys(&st, &keys).await?;
    badge(&st, user.id, &mut items)?;
    Ok(Json(
        json!({ "items": items, "tag": marks::normalize_tag(&tag), "offset": offset }),
    ))
}

/// Most common ingredient lines, for the ingredient browser.
async fn api_ingredients(
    State(st): State<St>,
    Query(qp): Query<SearchQ>,
) -> Result<Json<J>, AppError> {
    let conn = st.db.connect()?;
    let limit = qp.limit.unwrap_or(200).clamp(1, 1000);
    let q = format!(
        "%{}%",
        qp.q.clone().unwrap_or_default().trim().to_lowercase()
    );
    let mut rows = conn
        .query(
            "SELECT lower(line) AS l, COUNT(*) AS n FROM ingredients \
             WHERE lower(line) LIKE ?1 GROUP BY l ORDER BY n DESC LIMIT ?2",
            params![q, limit],
        )
        .await?;
    let mut items = Vec::new();
    while let Some(r) = rows.next().await? {
        items.push(json!({
            "line": db::val_str(&r.get_value(0)?),
            "count": db::val_i64(&r.get_value(1)?),
        }));
    }
    Ok(Json(json!({ "items": items })))
}

// ------------------------------------------------------- epub asset serving

fn book_path(st: &St, id: i64) -> Result<std::path::PathBuf, AppError> {
    st.books
        .get(&id)
        .cloned()
        .ok_or_else(|| AppError(anyhow::anyhow!("no such book")))
}

/// Read one entry out of an epub on the blocking pool.
async fn read_entry(path: std::path::PathBuf, entry: String) -> Result<Vec<u8>, AppError> {
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut e = Epub::open(&path)?;
        e.read(&entry)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(bytes)
}

fn bytes_response(name: &str, bytes: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    // Book contents never change under a given path.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800"),
    );
    (headers, bytes).into_response()
}

async fn asset(
    State(st): State<St>,
    Path((book_id, path)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    let p = book_path(&st, book_id)?;
    let bytes = read_entry(p, path.clone()).await?;
    Ok(bytes_response(&path, bytes))
}

async fn cover(State(st): State<St>, Path(book_id): Path<i64>) -> Result<Response, AppError> {
    let p = book_path(&st, book_id)?;
    let cover = tokio::task::spawn_blocking(move || -> Result<Option<(String, Vec<u8>)>> {
        let mut e = Epub::open(&p)?;
        let Some(c) = e.cover.clone() else {
            return Ok(None);
        };
        Ok(Some((c.clone(), e.read(&c)?)))
    })
    .await
    .map_err(anyhow::Error::from)??;

    match cover {
        Some((name, bytes)) => Ok(bytes_response(&name, bytes)),
        None => Ok((StatusCode::NOT_FOUND, "no cover").into_response()),
    }
}

/// Serve a book page, rewriting internal links so images and stylesheets
/// resolve through us instead of pointing at zip-relative paths.
async fn page(
    State(st): State<St>,
    Path((book_id, path)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    let p = book_path(&st, book_id)?;
    let raw = read_entry(p, path.clone()).await?;
    let html = String::from_utf8_lossy(&raw).into_owned();
    let base = crate::epub::dir_of_pub(&path);

    let rewritten = rewrite_page(&html, &base, book_id)?;
    Ok((
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        rewritten,
    )
        .into_response())
}

fn rewrite_page(html: &str, base: &str, book_id: i64) -> Result<String, AppError> {
    use lol_html::{HtmlRewriter, Settings, element};

    let settings = Settings::new()
        .append_element_content_handler(element!("img, image", |el| {
            for attr in ["src", "xlink:href", "href"] {
                if let Some(v) = el.get_attribute(attr) {
                    let t = resolve(base, &v);
                    el.set_attribute(attr, &format!("/asset/{book_id}/{t}"))
                        .ok();
                }
            }
            Ok(())
        }))
        .append_element_content_handler(element!("link[rel=stylesheet]", |el| {
            if let Some(v) = el.get_attribute("href") {
                let t = resolve(base, &v);
                el.set_attribute("href", &format!("/asset/{book_id}/{t}"))
                    .ok();
            }
            Ok(())
        }))
        .append_element_content_handler(element!("a[href]", |el| {
            let Some(v) = el.get_attribute("href") else {
                return Ok(());
            };
            if v.starts_with("http://") || v.starts_with("https://") || v.starts_with("mailto:") {
                el.set_attribute("target", "_blank").ok();
                return Ok(());
            }
            // Same-document jump: leave as-is.
            if v.starts_with('#') {
                return Ok(());
            }
            let target = resolve(base, &v);
            let frag = v
                .split_once('#')
                .map(|(_, f)| format!("#{f}"))
                .unwrap_or_default();
            el.set_attribute("href", &format!("/page/{book_id}/{target}{frag}"))
                .ok();
            Ok(())
        }))
        // Make the page usable inside the reader frame.
        .append_element_content_handler(element!("head", |el| {
            el.append(
                "<style>html,body{max-width:46rem;margin:0 auto;padding:1.2rem 1.4rem 4rem;}\
                 img{max-width:100%!important;height:auto!important;}\
                 body{background:#fff;}</style>",
                lol_html::html_content::ContentType::Html,
            );
            Ok(())
        }));

    let mut out = Vec::new();
    {
        let mut rewriter = HtmlRewriter::new(settings, |c: &[u8]| out.extend_from_slice(c));
        rewriter
            .write(html.as_bytes())
            .map_err(anyhow::Error::from)?;
        rewriter.end().map_err(anyhow::Error::from)?;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
