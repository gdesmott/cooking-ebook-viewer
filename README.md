# Cookbook Library

A small web app to browse, read and search a shelf of cooking EPUBs — 61 vegan
cookbooks from a Humble Bundle, in the original test setup.

- **Read a book** with its own layout, typography and photos intact.
- **Global recipe index** across every book (~4,900 recipes).
- **Full-text search** over recipe titles, ingredients and instructions, with
  the matching ingredient highlighted in the result.

Books are never unpacked: pages and images are read straight out of the `.epub`
zips at request time, so the 12 GB library stays where it is.

## Setup

The only requirement is a Rust toolchain.

```sh
cargo run --release -- index --library ~/books   # build library.db (~3 min for 61 books)
cargo run --release -- serve                     # http://127.0.0.1:8080
```

`--library` points at a directory of `.epub` files. The database defaults to
`library.db` and the server to `127.0.0.1:8080`; both are overridable:

```sh
cargo run --release -- index --library ~/books --db mylib.db
cargo run --release -- serve --db mylib.db --addr 0.0.0.0:8080
```

`cargo run --release -- stats` reports extraction quality: how many entries came
out without an ingredient list, and which ones.

## Accounts

Everything except the login form requires a signed-in session, so the library
isn't publicly readable. There is no sign-up page; accounts are managed from the
command line, and this works **while the server is running**:

```sh
cargo run --release -- user add <name>      # prompts for a password, twice
cargo run --release -- user list
cargo run --release -- user passwd <name>   # also signs out that user's sessions
cargo run --release -- user rm <name>
```

Piping a password works too, for scripts: `echo 'pw' | ... user add bob`.

Passwords are stored as Argon2id hashes. Sessions are random 32-byte tokens in
an `HttpOnly`, `SameSite=Lax` cookie, valid for 30 days and revocable by
changing the password or removing the user. Add `--secure-cookie` when serving
behind HTTPS so the cookie is not sent over plain HTTP.

Accounts live in **`auth.db`, separate from `library.db`** — indexing rebuilds
the library from scratch every run and would otherwise wipe them. Anything
durable and user-owned (notes, tags, favourites) belongs there too. Unlike the
library, it is plain SQLite: Turso holds an exclusive lock for the lifetime of a
connection, which would stop the CLI from adding a user while the server runs.

The NAS (or wherever the epubs live) must be mounted when the server runs, since
images are streamed from the archives on demand. `library.db` is a derived
artifact — delete it and re-run `index` at any time. `auth.db` is not: it holds
your accounts.

## Database

SQLite-compatible, via [Turso](https://github.com/tursodatabase/turso)
(`turso` crate, embedded, no server).

Turso does **not** implement FTS5. Search uses its Tantivy-backed full-text
index instead:

```sql
CREATE INDEX recipes_fts ON recipes USING fts (title, ingredients, body)
    WITH (weights = 'title=3.0,ingredients=2.0,body=1.0');
```

and the `fts_match` / `fts_score` (BM25) / `fts_highlight` functions. Two
consequences worth knowing:

- Custom index methods are still experimental, so the connection is opened with
  `.experimental_index_method(true)`.
- Once that index exists, **stock SQLite cannot open the file** — `sqlite3` and
  Python's `sqlite3` both reject the schema. Nothing is authored in the
  database, so this costs nothing here, but don't expect to inspect it with
  standard tooling. Use `stats`, or index to a throwaway DB, instead.

### Searching

Tantivy's query parser ORs bare terms, so `air fryer` would match any recipe
merely containing "air". Multi-word queries are rewritten to require every term
(`air AND fryer`), which is what searching for two ingredients means. Anything
using explicit query syntax is passed through untouched:

| query | meaning |
| --- | --- |
| `air fryer` | both terms (the default) |
| `"air fryer"` | exact phrase |
| `air OR fryer` | either term |
| `harissa NOT chickpea` | one term, excluding another |
| `tofu*` | prefix match |
| `title:harissa` | match in the title only |

### Schema

| table | contents |
| --- | --- |
| `books` | one row per epub: title, author, cover path, absolute path on disk |
| `recipes` | title, chapter, source document + anchor, hero image, yield, time, ingredients, instructions |
| `ingredients` | one row per ingredient line, for ingredient-level queries |

## How recipes are found

The epubs come from a dozen publishers with no shared markup convention, so
extraction is heuristic:

1. **Which entries are recipes** — the NCX table of contents is nested
   chapter → recipe in most books, but two are flat and one is three deep. The
   rule that covers all three shapes: *leaf* TOC nodes are recipes, minus a
   front/back-matter blocklist (covers, dedications, indexes…).
2. **Splitting shared documents** — some books put many recipes in one XHTML
   file, distinguished only by the anchor each TOC entry targets. Documents are
   parsed in order and split at those anchors, so a recipe gets its own
   ingredients rather than the whole file's.
3. **Which lines are ingredients** — there is no common class name
   (`p.ing`, `p.res`, `p.ingredient`, `p.recp`, …). For each book, paragraphs
   that *look* like quantities ("1 cup (240 ml) …") are scored, the dominant
   CSS class among them is inferred, and that class is then used to pick up the
   lines a regex alone would miss ("Salt and pepper to taste").
4. **Yield and time** come from the book's own marker block (`div.serve`,
   `p.makes`, …) rather than the instructions, which otherwise contribute stray
   durations.

Entries that end up with no ingredients (~2%, almost all genuine prose sections
like "My Food Philosophy") still appear in the book reader, but are kept out of
the recipe index and search results.

## Layout

```
src/auth.rs      accounts, password hashing, sessions (separate SQLite db)
src/epub.rs      zip + OPF metadata + NCX/nav table of contents
src/extract.rs   recipe extraction heuristics
src/db.rs        Turso schema, FTS index
src/indexer.rs   walks the library, populates the DB, `stats`
src/server.rs    axum routes, on-demand page/asset serving with URL rewriting
static/          single-page frontend (no build step, no dependencies)
```

Book pages are served through `/page/{book}/{path}`, which rewrites `img`,
`link` and `a` hrefs to point back through the app, and renders inside an
iframe so the book's CSS can't leak into the app's.
