# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

A web app to browse, read and search a shelf of EPUB cookbooks. See `README.md`
for the user-facing description and the extraction heuristics in prose.

## Commands

```sh
cargo run --release -- index --library <dir>   # rebuild library.db (~3 min for 61 books)
cargo run --release -- serve                   # http://127.0.0.1:8080
cargo run --release -- stats                   # extraction quality: entries with no ingredients
cargo run --release -- user add <name>         # accounts; also list / passwd / rm
```

`--library` is required and has no default. Other flags: `--db <file>`
(default `library.db`), `--auth-db <file>` (default `auth.db`),
`--addr <host:port>` (default `127.0.0.1:8080`), `--secure-cookie`.

Use `--release`: debug builds parse 61 large zips slowly enough to matter.

There is **no test suite**. Verification is manual: `stats` for extraction
quality, `curl /api/search` for relevance, and a browser for the UI.

### Gotchas when iterating

- **`static/` is embedded via `include_str!`** — editing `app.js`/`app.css`/
  `index.html` does nothing until you `cargo build`. Restarting alone won't do it.
- **Changing `src/extract.rs` requires a re-index** to see any effect.
- **A running server holds a lock on `library.db`.** Stop it before indexing, or
  index to a scratch `--db` and swap the file (`library.db` *and*
  `library.db-wal`) in. `auth.db` is not affected — that is why it is SQLite.
- Index to a throwaway `--db` when experimenting, then compare with `stats`
  before overwriting a good `library.db`.

## Two databases

- **`library.db`** (Turso) — derived from the epubs, rebuilt from scratch by
  every `index`, gitignored. Never store anything authored here.
- **`auth.db`** (plain SQLite, `src/auth.rs`) — accounts and sessions, and the
  right home for future user-owned data (notes, tags, favourites).

They are deliberately different engines. Turso holds an **exclusive file lock
for the lifetime of a connection**, so with Turso the running server made
`user add` fail — and managing accounts while serving is the point of the CLI.
SQLite in WAL mode with a busy timeout handles both processes. The auth
connection is a `Mutex<rusqlite::Connection>` in `AppState`; take the guard in
a scope and drop it before any `.await`, or the future stops being `Send`.

## Auth

Every route except `/login` sits behind the `require_auth` middleware: `/api/*`
gets a 401 so the frontend can redirect, everything else gets a 303 to the login
form. Passwords are Argon2id; sessions are random 32-byte tokens in an
`HttpOnly`/`SameSite=Lax` cookie. `--secure-cookie` adds `Secure` for HTTPS.

When adding a route, put it inside the protected router unless it genuinely
must be public, and keep the `no_store` vs cacheable split correct.

Note that **recipe ids are reassigned on every re-index** (the indexer numbers
them sequentially). Before building notes/tags/favourites, give recipes a stable
key — book filename plus doc plus anchor, say — or user data will silently point
at the wrong recipes after the next index.

## Turso, and how it differs from SQLite

Storage is the embedded `turso` crate. Four things bite:

1. **No FTS5.** Search uses Turso's Tantivy-backed index
   (`CREATE INDEX ... USING fts`, in `db::FTS_INDEX`) and the `fts_match` /
   `fts_score` / `fts_highlight` functions. FTS5 syntax will not work.
2. **Every connection needs `.experimental_index_method(true)`** (see `db::open`
   and `server::run`) or any statement touching that index fails to parse.
3. **Stock SQLite cannot open `library.db` once the FTS index exists** — `sqlite3`
   and Python's `sqlite3` both reject the schema outright. You cannot inspect
   `library.db` with standard tooling; use `stats`, or index to a scratch DB.
4. **`params!` expands to a fixed-size array**, so `match` arms that bind
   different numbers of parameters won't typecheck. Build a `Vec<Value>` and
   push conditionally — see `api_search`.

The database is a **derived artifact**: nothing is authored in it, `index`
always rebuilds from scratch, and it is gitignored. Never treat it as a source
of truth to be migrated.

## Architecture

```
src/auth.rs      accounts, Argon2id hashing, sessions (plain SQLite)
src/epub.rs      zip access, OPF metadata, NCX/nav TOC parsing
src/extract.rs   TOC -> recipes; ingredient/body/meta classification
src/db.rs        schema + FTS index DDL + Value accessors
src/indexer.rs   walks the library, populates the DB; `stats`
src/server.rs    axum routes, search queries, on-demand epub serving
static/          single-page frontend, no build step, no dependencies
```

### Extraction invariants

These encode real corpus quirks; changing them regresses specific books.

- **Leaf TOC nodes are recipes.** The corpus has three TOC shapes — nested
  chapter→recipe (58 books), flat (2), three-deep (1). `Epub::leaves()` handles
  all three; anything keyed off "depth 2" will break the outliers.
- **Documents are split at TOC anchors.** Several books put many recipes in one
  XHTML file distinguished only by the anchor each TOC entry targets. `parse_doc`
  walks the DOM in document order tracking the most recent matching `id`. Parsing
  a whole file per recipe gives every recipe the file's full ingredient list —
  this was a real bug.
- **Ingredient CSS classes are inferred per book.** There is no shared class name
  (`p.ing`, `p.res`, `p.ingredient`, `p.recp`, …). `detect_ingredient_classes`
  scores paragraphs that look like quantities, then the dominant class is reused
  to catch lines a regex misses ("Salt and pepper to taste").
- **Yield/time come only from the book's marker block** (`div.serve`, `p.makes`,
  classes containing serve/make/yield/prep/time), never the instructions, which
  otherwise contribute stray durations.
- **Entries with no ingredients are kept** (~2%, mostly genuine prose sections
  like "My Food Philosophy"). They appear in the reader TOC but are excluded
  from search and the recipe index by `WHERE r.ingredients != ''`.
- The indexer **canonicalizes epub paths** — the server may run from a different
  working directory than the indexer did.

### Search semantics

Tantivy's parser **ORs bare terms**, so a raw `air fryer` matches anything
containing "air". Two helpers in `server.rs` handle this, and they must stay in sync:

- `build_fts_query` — joins multi-word queries with `AND`; passes through
  anything using explicit syntax (`"phrase"`, `AND`/`OR`/`NOT`, `prefix*`,
  `field:term`, `+`/`-`).
- `build_highlight_query` — terms only, operators stripped. `fts_highlight`
  marks whatever it is given, so feeding it the match query highlights the
  literal "and" in "Salt and pepper" — and since `make_snippet` centres its
  window on the first `<mark>`, genuine matches then display an irrelevant
  fragment.

They are bound as separate parameters (`?3` match/score, `?4` highlight).

### Serving epub content

Nothing is unpacked. `/asset/{book}/{path}` and `/page/{book}/{path}` open the
zip per request on `spawn_blocking`. `/page` rewrites `img`, `link` and `a`
hrefs through `lol_html` to point back through the app, and the frontend loads
it in an **iframe** so the book's own CSS cannot leak into the app's.

`AppState.books` maps book id → epub path, resolved once at startup; the epubs
must be mounted while the server runs.

### Cache headers

`server::run` builds two routers: UI and `/api/*` get `no-store` (they change
on re-index, and browsers otherwise heuristically cache them and replay stale
results without ever contacting the server), while `/asset`, `/cover` and
`/page` are immutable per path and cached for a week. Keep new routes on the
correct side.

### Text handling

Recipe text is full of multi-byte characters (`½`, `·`, `–`). Slice by chars,
not bytes — byte-offset arithmetic on this corpus panics in production
(`make_snippet` did).
