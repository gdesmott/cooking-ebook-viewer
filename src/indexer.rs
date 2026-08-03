//! Walks the epub library and populates the Turso database.

use crate::{db, epub::Epub, extract};
use anyhow::Result;
use std::path::{Path, PathBuf};
use turso::params;

pub fn find_epubs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("epub"))
        {
            // Store absolute paths: the server may run from another directory.
            out.push(p.canonicalize().unwrap_or(p));
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run(library: &Path, db_path: &str) -> Result<()> {
    // The database is a derived artifact: always rebuild from scratch so a
    // re-run can never leave stale rows behind.
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_path}{suffix}"));
    }

    let conn = db::open(db_path).await?;
    db::init(&conn).await?;

    let files = find_epubs(library)?;
    println!("Indexing {} epubs from {}", files.len(), library.display());

    let mut book_id: i64 = 0;
    let mut recipe_id: i64 = 0;
    let mut ing_id: i64 = 0;
    let mut total_recipes = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();

    conn.execute("BEGIN", ()).await?;
    for (i, path) in files.iter().enumerate() {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        let mut epub = match Epub::open(path) {
            Ok(e) => e,
            Err(e) => {
                failures.push((name.clone(), e.to_string()));
                println!("[{}/{}] {name}: FAILED to open: {e}", i + 1, files.len());
                continue;
            }
        };

        let recipes = match extract::extract_book(&mut epub) {
            Ok(r) => r,
            Err(e) => {
                failures.push((name.clone(), e.to_string()));
                println!("[{}/{}] {name}: FAILED to extract: {e}", i + 1, files.len());
                continue;
            }
        };

        book_id += 1;
        conn.execute(
            "INSERT INTO books (id, filename, path, title, author, language, cover, n_recipes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                book_id,
                name.clone(),
                path.to_string_lossy().into_owned(),
                epub.title.clone(),
                epub.author.clone(),
                epub.language.clone(),
                epub.cover.clone(),
                recipes.iter().filter(|r| !r.ingredients.is_empty()).count() as i64
            ],
        )
        .await?;

        for r in &recipes {
            recipe_id += 1;
            conn.execute(
                "INSERT INTO recipes (id, book_id, title, chapter, doc, anchor, position, \
                 image, yield_text, time_text, ingredients, body) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    recipe_id,
                    book_id,
                    r.title.clone(),
                    r.chapter.clone(),
                    r.doc.clone(),
                    r.anchor.clone(),
                    r.position,
                    r.image.clone(),
                    r.yield_text.clone(),
                    r.time_text.clone(),
                    r.ingredients.join("\n"),
                    r.body.clone()
                ],
            )
            .await?;

            for line in &r.ingredients {
                ing_id += 1;
                conn.execute(
                    "INSERT INTO ingredients (id, recipe_id, book_id, line) VALUES (?1, ?2, ?3, ?4)",
                    params![ing_id, recipe_id, book_id, line.clone()],
                )
                .await?;
            }
        }

        total_recipes += recipes.len();
        let with_ing = recipes.iter().filter(|r| !r.ingredients.is_empty()).count();
        println!(
            "[{}/{}] {:<46} {:>4} recipes ({} with ingredients)",
            i + 1,
            files.len(),
            truncate(&epub.title, 46),
            recipes.len(),
            with_ing
        );
    }
    conn.execute("COMMIT", ()).await?;

    println!("\nBuilding full-text index...");
    conn.execute(db::FTS_INDEX, ()).await?;

    println!(
        "\nDone: {book_id} books, {total_recipes} recipes, {ing_id} ingredient lines -> {db_path}"
    );
    if !failures.is_empty() {
        println!("\n{} book(s) failed:", failures.len());
        for (f, e) in &failures {
            println!("  {f}: {e}");
        }
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

/// Report extraction quality: which books produced recipes without an
/// ingredient list, so heuristics can be checked against real output.
pub async fn stats(db_path: &str) -> Result<()> {
    let conn = db::open(db_path).await?;

    let mut rows = conn
        .query(
            "SELECT COUNT(*), SUM(CASE WHEN ingredients = '' THEN 1 ELSE 0 END) FROM recipes",
            (),
        )
        .await?;
    if let Some(r) = rows.next().await? {
        let total = crate::db::val_i64(&r.get_value(0)?).unwrap_or(0);
        let empty = crate::db::val_i64(&r.get_value(1)?).unwrap_or(0);
        println!(
            "{total} entries, {} with ingredients, {empty} without ({:.1}%)",
            total - empty,
            empty as f64 * 100.0 / total.max(1) as f64
        );
    }

    println!("\nEntries without ingredients (candidates for prose sections):");
    let mut rows = conn
        .query(
            "SELECT r.title, b.title, LENGTH(r.body) FROM recipes r \
             JOIN books b ON b.id = r.book_id WHERE r.ingredients = '' \
             ORDER BY b.title, r.position",
            (),
        )
        .await?;
    let mut n = 0;
    while let Some(r) = rows.next().await? {
        let t = crate::db::val_str(&r.get_value(0)?).unwrap_or_default();
        let b = crate::db::val_str(&r.get_value(1)?).unwrap_or_default();
        let len = crate::db::val_i64(&r.get_value(2)?).unwrap_or(0);
        println!(
            "  {:<52} | {:<28} | {len} chars",
            truncate(&t, 52),
            truncate(&b, 28)
        );
        n += 1;
    }
    println!("\n{n} shown");
    Ok(())
}
