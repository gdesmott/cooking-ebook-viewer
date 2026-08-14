//! Axum HTTP server. Book pages and images are streamed straight out of the
//! epub zips on demand, so nothing is unpacked to disk.

use crate::auth;
use crate::db;
use crate::epub::{Epub, resolve};
use anyhow::Result;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
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

async fn api_search(State(st): State<St>, Query(qp): Query<SearchQ>) -> Result<Json<J>, AppError> {
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
            items.push(hit_json(&r)?);
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
            items.push(hit_json(&r)?);
        }
    }

    Ok(Json(json!({ "items": items, "q": q, "offset": offset })))
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

fn hit_json(r: &turso::Row) -> Result<J, AppError> {
    Ok(json!({
        "id": db::val_i64(&r.get_value(0)?),
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

async fn api_recipe(State(st): State<St>, Path(id): Path<i64>) -> Result<Json<J>, AppError> {
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
    Ok(Json(json!({
        "id": db::val_i64(&r.get_value(0)?),
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
    })))
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
