//! Recipe extraction.
//!
//! The books come from a dozen different publishers, so there is no single
//! class name for ingredient lines (`p.ing`, `p.res`, `p.ingredient`,
//! `p.recp`, ...). Instead we detect, per book, which class is *used* for
//! ingredient lines by scoring paragraphs that look like quantities, then
//! reuse that class to pick up the lines the regex alone would miss
//! ("Salt and pepper to taste").

use crate::epub::{Epub, resolve};
use anyhow::Result;
use regex::Regex;
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug, Default)]
pub struct Recipe {
    pub title: String,
    pub chapter: Option<String>,
    pub doc: String,
    pub anchor: Option<String>,
    pub position: i64,
    pub image: Option<String>,
    pub yield_text: Option<String>,
    pub time_text: Option<String>,
    pub ingredients: Vec<String>,
    pub body: String,
}

/// Labels and file stems that are book furniture rather than recipes.
static SKIP_LABELS: &[&str] = &[
    "cover",
    "title page",
    "copyright",
    "copyright notice",
    "dedication",
    "acknowledgments",
    "acknowledgements",
    "about the author",
    "about the authors",
    "index",
    "contents",
    "table of contents",
    "introduction",
    "foreword",
    "preface",
    "resources",
    "newsletter",
    "notice",
    "epigraph",
    "half title",
    "also by",
    "praise for",
    "sign up",
    "buy the book",
    "further reading",
    "conversion chart",
    "metric conversions",
    "credits",
    "photo credits",
    "colophon",
    "endnotes",
    "bibliography",
    "afterword",
    "appendix",
    "glossary",
];

static SKIP_STEMS: &[&str] = &[
    "cvi",
    "cover",
    "tp",
    "titlepage",
    "cop",
    "copyright",
    "ded",
    "ack",
    "auth",
    "bm",
    "toc",
    "nav",
    "notice",
    "newsletter",
    "index",
    "fm",
    "epi",
    "adcard",
];

static UNITS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)\b(
        cups?|c\.|tbsp|tsp|tablespoons?|teaspoons?|ounces?|oz|pounds?|lbs?|
        grams?|g|kg|kilograms?|ml|milliliters?|liters?|litres?|l|
        quarts?|qt|pints?|pt|gallons?|
        cloves?|cans?|jars?|packages?|pkg|boxes?|bunch(?:es)?|sprigs?|stalks?|
        heads?|slices?|pinch(?:es)?|dash(?:es)?|handfuls?|sticks?|ears?|
        fillets?|blocks?|bags?|sheets?|drops?
        )\b",
    )
    .unwrap()
});

static STARTS_QTY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)^\s*(
        [\d\u{00BC}-\u{00BE}\u{2150}-\u{215E}]|
        (a|an|one|two|three|four|five|six|seven|eight|nine|ten|half)\s|
        (pinch|dash|handful|salt|pepper|freshly|juice|zest|sea\s+salt|
         kosher|olive\s+oil|water|optional|few|several)\b
        )",
    )
    .unwrap()
});

static YIELD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)\b((?:serves|makes|yields?)\s+(?:about\s+|approximately\s+)?
          [\d\u{00BC}-\u{00BE}\u{2150}-\u{215E}]+
          (?:\s*(?:to|[-\u{2013}\u{2014}])\s*[\d\u{00BC}-\u{00BE}\u{2150}-\u{215E}]+)?
          (?:\s+[\w()\u{00BD}-]+){0,3})",
    )
    .unwrap()
});

static TIME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)\b(\d+(?:\s*(?:to|[-\u{2013}\u{2014}])\s*\d+)?\s*
          (?:minutes?|mins?|hours?|hrs?)
          (?:\s+(?:to\s+)?(?:prepare|cook|bake|chill|rest|marinate|total|active))?)",
    )
    .unwrap()
});

static SEL_P: LazyLock<Selector> = LazyLock::new(|| Selector::parse("p, li, div").unwrap());

fn norm_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Tidy a yield/time fragment: trailing punctuation and a dangling "(" left
/// behind when the match stops mid-parenthetical both look sloppy in the UI.
fn clean_meta(s: &str) -> String {
    let mut s = norm_ws(s)
        .trim_end_matches([',', ';', ':', '-', '–', '(', ' '])
        .to_string();
    if s.matches('(').count() > s.matches(')').count()
        && let Some(i) = s.rfind('(')
    {
        s.truncate(i);
    }
    norm_ws(&s)
}

fn looks_like_ingredient(t: &str) -> bool {
    let len = t.chars().count();
    if !(2..=180).contains(&len) {
        return false;
    }
    // Instruction sentences are long and usually contain a verb phrase; the
    // length cap plus a unit/quantity cue separates them well in practice.
    STARTS_QTY.is_match(t) && (UNITS.is_match(t) || len < 60)
}

fn is_skippable(label: &str, doc: &str) -> bool {
    let l = label.trim().to_lowercase();
    if SKIP_LABELS
        .iter()
        .any(|s| l == *s || l.starts_with(&format!("{s}:")))
    {
        return true;
    }
    let stem = doc
        .rsplit('/')
        .next()
        .unwrap_or(doc)
        .split('.')
        .next()
        .unwrap_or("")
        .to_lowercase();
    SKIP_STEMS.contains(&stem.as_str())
}

/// Text of an element with nested markup flattened.
fn el_text(el: scraper::ElementRef) -> String {
    norm_ws(&el.text().collect::<String>())
}

/// Pass 1: find which CSS classes this book uses for ingredient lines.
fn detect_ingredient_classes(docs: &[String]) -> Vec<String> {
    let mut hits: HashMap<String, usize> = HashMap::new();
    let mut totals: HashMap<String, usize> = HashMap::new();

    for raw in docs {
        let html = Html::parse_document(raw);
        for el in html.select(&SEL_P) {
            // Only leaf-ish blocks; a <div> wrapping the whole recipe would
            // otherwise swallow everything.
            if el.children().any(|c| {
                c.value()
                    .as_element()
                    .is_some_and(|e| matches!(e.name(), "p" | "div" | "ul" | "ol"))
            }) {
                continue;
            }
            let Some(class) = el.value().attr("class") else {
                continue;
            };
            let t = el_text(el);
            if t.is_empty() {
                continue;
            }
            for cls in class.split_whitespace() {
                *totals.entry(cls.to_string()).or_default() += 1;
                if looks_like_ingredient(&t) {
                    *hits.entry(cls.to_string()).or_default() += 1;
                }
            }
        }
    }

    // Keep classes that are both frequently ingredient-shaped and mostly
    // ingredient-shaped, so body-text classes don't sneak in.
    let mut scored: Vec<(String, usize)> = hits
        .iter()
        .filter(|(cls, h)| {
            let h = **h;
            let total = totals.get(*cls).copied().unwrap_or(1);
            h >= 5 && (h as f64 / total as f64) >= 0.55
        })
        .map(|(c, h)| (c.clone(), *h))
        .collect();
    scored.sort_by_key(|(_, h)| std::cmp::Reverse(*h));

    let Some((_, top)) = scored.first().cloned() else {
        return Vec::new();
    };
    scored
        .into_iter()
        .filter(|(_, h)| *h * 4 >= top)
        .map(|(c, _)| c)
        .collect()
}

/// Extract every recipe from one book.
pub fn extract_book(epub: &mut Epub) -> Result<Vec<Recipe>> {
    // Candidate recipe entries: TOC leaves that aren't front/back matter.
    let candidates: Vec<(Option<String>, String, String, Option<String>)> = epub
        .leaves()
        .into_iter()
        .filter(|(_, np)| !np.label.trim().is_empty() && !is_skippable(&np.label, &np.doc))
        .map(|(chapter, np)| (chapter, np.label.clone(), np.doc.clone(), np.anchor.clone()))
        .collect();

    // Load each distinct document once.
    let mut doc_cache: HashMap<String, String> = HashMap::new();
    for (_, _, doc, _) in &candidates {
        if !doc_cache.contains_key(doc)
            && let Ok(text) = epub.read_text(doc)
        {
            doc_cache.insert(doc.clone(), text);
        }
    }

    let sample: Vec<String> = doc_cache.values().cloned().collect();
    let ing_classes = detect_ingredient_classes(&sample);

    // Several books put many recipes in one XHTML file, distinguished only by
    // the anchor each TOC entry points at. Parse each document once, split by
    // those anchors, so a recipe gets its own ingredients and not the file's.
    let mut anchors_by_doc: HashMap<String, Vec<String>> = HashMap::new();
    for (_, _, doc, anchor) in &candidates {
        if let Some(a) = anchor {
            anchors_by_doc
                .entry(doc.clone())
                .or_default()
                .push(a.clone());
        }
    }
    let mut parsed_docs: HashMap<String, HashMap<String, Parsed>> = HashMap::new();
    for (doc, raw) in &doc_cache {
        let anchors = anchors_by_doc.get(doc).cloned().unwrap_or_default();
        parsed_docs.insert(doc.clone(), parse_doc(raw, &ing_classes, doc, &anchors));
    }

    let mut out = Vec::new();
    for (pos, (chapter, label, doc, anchor)) in candidates.into_iter().enumerate() {
        let Some(sections) = parsed_docs.get(&doc) else {
            continue;
        };
        let mut r = section_to_recipe(sections, anchor.as_deref());
        r.title = label;
        r.chapter = chapter;
        r.doc = doc;
        r.anchor = anchor;
        r.position = pos as i64;
        // Recipes have ingredients; entries without any are prose sections.
        if r.ingredients.is_empty() && r.body.chars().count() < 200 {
            continue;
        }
        out.push(r);
    }
    Ok(out)
}

/// Content of one recipe section within a document.
#[derive(Default, Clone)]
struct Parsed {
    ingredients: Vec<String>,
    body: Vec<String>,
    meta: Vec<String>,
    image: Option<String>,
}

/// Parse a document into sections keyed by anchor id. Content appearing before
/// the first anchor is stored under "", which is also what single-recipe
/// documents use.
fn parse_doc(
    raw: &str,
    ing_classes: &[String],
    doc: &str,
    anchors: &[String],
) -> HashMap<String, Parsed> {
    let html = Html::parse_document(raw);
    let base = crate::epub::dir_of_pub(doc);
    let targets: std::collections::HashSet<&str> = anchors.iter().map(String::as_str).collect();

    let mut sections: HashMap<String, Parsed> = HashMap::new();
    let mut current = String::new();

    // descendants() is document order, so tracking the most recent anchor id
    // tells us which recipe each block belongs to.
    for node in html.root_element().descendants() {
        let Some(el) = scraper::ElementRef::wrap(node) else {
            continue;
        };

        if let Some(id) = el.value().attr("id")
            && targets.contains(id)
        {
            current = id.to_string();
        }

        let name = el.value().name();

        if name == "img" {
            if let Some(src) = el.value().attr("src") {
                let p = resolve(&base, src);
                let f = p.rsplit('/').next().unwrap_or("").to_string();
                // Skip decorative ornaments, which are tiny shared images.
                let decorative =
                    f.starts_with("comn") || f.contains("ornament") || f.contains("logo");
                let entry = sections.entry(current.clone()).or_default();
                if !decorative && entry.image.is_none() {
                    entry.image = Some(p);
                }
            }
            continue;
        }

        if !matches!(name, "p" | "li" | "div") {
            continue;
        }
        // Skip containers; their children carry the actual text.
        if el.children().any(|c| {
            c.value()
                .as_element()
                .is_some_and(|e| matches!(e.name(), "p" | "div" | "ul" | "ol"))
        }) {
            continue;
        }
        let t = el_text(el);
        if t.is_empty() {
            continue;
        }

        // The marker class often sits on a wrapping <div>, so check ancestors.
        let is_meta = el
            .ancestors()
            .filter_map(scraper::ElementRef::wrap)
            .chain(std::iter::once(el))
            .filter_map(|a| a.value().attr("class"))
            .any(|c| {
                let c = c.to_lowercase();
                ["serve", "make", "yield", "prep", "time"]
                    .iter()
                    .any(|k| c.contains(k))
            });

        let by_class = el
            .value()
            .attr("class")
            .map(|c| {
                c.split_whitespace()
                    .any(|cls| ing_classes.iter().any(|i| i == cls))
            })
            .unwrap_or(false);

        let entry = sections.entry(current.clone()).or_default();
        if is_meta {
            entry.meta.push(t.clone());
        }
        if by_class || (ing_classes.is_empty() && looks_like_ingredient(&t)) {
            entry.ingredients.push(t);
        } else if !is_meta {
            entry.body.push(t);
        }
    }

    sections
}

/// Build a recipe from the section its TOC entry points at, falling back to the
/// whole document when the anchor is absent or unknown.
fn section_to_recipe(sections: &HashMap<String, Parsed>, anchor: Option<&str>) -> Recipe {
    let merged;
    let p = match anchor.and_then(|a| sections.get(a)) {
        Some(p) => p,
        None => {
            let mut acc = Parsed::default();
            let mut keys: Vec<&String> = sections.keys().collect();
            keys.sort();
            for k in keys {
                let s = &sections[k];
                acc.ingredients.extend(s.ingredients.iter().cloned());
                acc.body.extend(s.body.iter().cloned());
                acc.meta.extend(s.meta.iter().cloned());
                if acc.image.is_none() {
                    acc.image = s.image.clone();
                }
            }
            merged = acc;
            &merged
        }
    };

    // Prefer the dedicated block; fall back to the first paragraph, which is
    // where books without a marker class tend to state the yield.
    let meta_text = if p.meta.is_empty() {
        p.body.first().cloned().unwrap_or_default()
    } else {
        p.meta.join(" \u{2022} ")
    };
    let yield_text = YIELD_RE
        .captures(&meta_text)
        .map(|c| clean_meta(&c[1]))
        .filter(|s| (3..40).contains(&s.chars().count()));
    let time_text = TIME_RE
        .captures(&meta_text)
        .map(|c| clean_meta(&c[1]))
        .filter(|s| (3..34).contains(&s.chars().count()));

    Recipe {
        image: p.image.clone(),
        yield_text,
        time_text,
        ingredients: p.ingredients.clone(),
        body: p.body.join("\n\n"),
        ..Default::default()
    }
}
