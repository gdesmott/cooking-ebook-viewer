//! Minimal EPUB reader: opens the zip, parses OPF metadata/manifest and the
//! NCX (or nav) table of contents. Everything is read on demand so we never
//! unpack the 12 GB library to disk.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub struct Epub {
    zip: zip::ZipArchive<BufReader<File>>,
    pub title: String,
    pub author: Option<String>,
    pub language: Option<String>,
    pub cover: Option<String>,
    pub toc: Vec<NavPoint>,
}

#[derive(Debug, Clone)]
pub struct NavPoint {
    pub label: String,
    /// Zip path of the document this entry points at.
    pub doc: String,
    /// Fragment after `#`, if any.
    pub anchor: Option<String>,
    pub children: Vec<NavPoint>,
}

impl NavPoint {
    /// Leaf entries are the recipes: this holds for the two-level books
    /// (chapter -> recipe), the three-level one, and the flat ones alike.
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// Join `href` onto the directory of `base` and normalise `..`/`.` away.
pub fn resolve(base_dir: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap_or(href);
    let href = percent_encoding::percent_decode_str(href)
        .decode_utf8_lossy()
        .into_owned();
    if base_dir.is_empty() {
        return normalize(&href);
    }
    normalize(&format!("{base_dir}/{href}"))
}

fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

fn dir_of(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

/// Directory component of a zip path, for resolving relative hrefs inside a
/// content document.
pub fn dir_of_pub(p: &str) -> String {
    dir_of(p)
}

impl Epub {
    pub fn open(path: &Path) -> Result<Epub> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut zip = zip::ZipArchive::new(BufReader::new(file))
            .with_context(|| format!("read zip {}", path.display()))?;

        let container = read_from(&mut zip, "META-INF/container.xml")?;
        let container = String::from_utf8_lossy(&container).into_owned();
        let cdoc = roxmltree::Document::parse(&container).context("parse container.xml")?;
        let opf_path = cdoc
            .descendants()
            .find(|n| n.has_tag_name("rootfile"))
            .and_then(|n| n.attribute("full-path"))
            .ok_or_else(|| anyhow!("no rootfile in container.xml"))?
            .to_string();
        let opf_dir = dir_of(&opf_path);

        let opf_raw = read_from(&mut zip, &opf_path)?;
        let opf_raw = String::from_utf8_lossy(&opf_raw).into_owned();
        let opf = roxmltree::Document::parse(&opf_raw).context("parse opf")?;

        let dc = |tag: &str| -> Option<String> {
            opf.descendants()
                .find(|n| {
                    n.has_tag_name(tag)
                        && n.tag_name()
                            .namespace()
                            .is_some_and(|ns| ns.contains("dc/elements"))
                })
                .and_then(|n| n.text())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };

        let title = dc("title").unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".into())
        });

        // manifest: id -> (zip path, media-type, properties)
        let mut manifest: HashMap<String, (String, String, String)> = HashMap::new();
        let mut href_to_id: HashMap<String, String> = HashMap::new();
        for item in opf.descendants().filter(|n| n.has_tag_name("item")) {
            let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) else {
                continue;
            };
            let full = resolve(&opf_dir, href);
            let mt = item.attribute("media-type").unwrap_or_default().to_string();
            let props = item.attribute("properties").unwrap_or_default().to_string();
            href_to_id.insert(full.clone(), id.to_string());
            manifest.insert(id.to_string(), (full, mt, props));
        }

        // cover: prefer <meta name="cover" content="id">, else properties="cover-image"
        let cover = opf
            .descendants()
            .find(|n| n.has_tag_name("meta") && n.attribute("name") == Some("cover"))
            .and_then(|n| n.attribute("content"))
            .and_then(|id| manifest.get(id))
            .or_else(|| {
                manifest.values().find(|(_, mt, props)| {
                    props.contains("cover-image") && mt.starts_with("image/")
                })
            })
            .map(|(p, _, _)| p.clone());

        let spine_el = opf.descendants().find(|n| n.has_tag_name("spine"));

        // Table of contents: NCX first (it carries the nesting), then EPUB3 nav.
        let ncx_path = spine_el
            .and_then(|s| s.attribute("toc"))
            .and_then(|id| manifest.get(id))
            .map(|(p, _, _)| p.clone())
            .or_else(|| {
                manifest
                    .values()
                    .find(|(_, mt, _)| mt == "application/x-dtbncx+xml")
                    .map(|(p, _, _)| p.clone())
            });

        let mut toc = Vec::new();
        if let Some(ncx_path) = &ncx_path
            && let Ok(bytes) = read_from(&mut zip, ncx_path)
        {
            let raw = String::from_utf8_lossy(&bytes).into_owned();
            if let Ok(doc) = roxmltree::Document::parse(&raw) {
                let base = dir_of(ncx_path);
                if let Some(navmap) = doc.descendants().find(|n| n.has_tag_name("navMap")) {
                    for np in navmap.children().filter(|n| n.has_tag_name("navPoint")) {
                        if let Some(p) = parse_navpoint(np, &base) {
                            toc.push(p);
                        }
                    }
                }
            }
        }
        if toc.is_empty() {
            let nav_path = manifest
                .values()
                .find(|(_, _, props)| props.contains("nav"))
                .map(|(p, _, _)| p.clone());
            if let Some(nav_path) = nav_path
                && let Ok(bytes) = read_from(&mut zip, &nav_path)
            {
                let raw = String::from_utf8_lossy(&bytes).into_owned();
                if let Ok(doc) = roxmltree::Document::parse(&raw) {
                    let base = dir_of(&nav_path);
                    if let Some(ol) = doc
                        .descendants()
                        .find(|n| n.has_tag_name("nav"))
                        .and_then(|n| n.children().find(|c| c.has_tag_name("ol")))
                    {
                        toc = parse_nav_ol(ol, &base);
                    }
                }
            }
        }

        Ok(Epub {
            zip,
            title,
            author: dc("creator"),
            language: dc("language"),
            cover,
            toc,
        })
    }

    pub fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        read_from(&mut self.zip, path)
    }

    pub fn read_text(&mut self, path: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.read(path)?).into_owned())
    }

    /// Flatten the TOC to leaf entries, remembering the nearest ancestor label
    /// as the chapter/section name.
    pub fn leaves(&self) -> Vec<(Option<String>, &NavPoint)> {
        fn walk<'a>(
            nodes: &'a [NavPoint],
            parent: Option<&str>,
            out: &mut Vec<(Option<String>, &'a NavPoint)>,
        ) {
            for n in nodes {
                if n.is_leaf() {
                    out.push((parent.map(|s| s.to_string()), n));
                } else {
                    // A node with children is a chapter heading; it may also
                    // point at real content, but its leaves are the recipes.
                    walk(&n.children, Some(&n.label), out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.toc, None, &mut out);
        out
    }
}

fn read_from(zip: &mut zip::ZipArchive<BufReader<File>>, path: &str) -> Result<Vec<u8>> {
    let mut f = zip
        .by_name(path)
        .with_context(|| format!("entry not found: {path}"))?;
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn parse_navpoint(np: roxmltree::Node, base: &str) -> Option<NavPoint> {
    let label = np
        .descendants()
        .find(|n| n.has_tag_name("navLabel"))
        .and_then(|n| n.descendants().find(|c| c.has_tag_name("text")))
        .and_then(|n| n.text())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let src = np
        .children()
        .find(|n| n.has_tag_name("content"))
        .and_then(|n| n.attribute("src"))?;
    let anchor = src.split_once('#').map(|(_, a)| a.to_string());
    let children = np
        .children()
        .filter(|n| n.has_tag_name("navPoint"))
        .filter_map(|c| parse_navpoint(c, base))
        .collect();
    Some(NavPoint {
        label,
        doc: resolve(base, src),
        anchor,
        children,
    })
}

fn parse_nav_ol(ol: roxmltree::Node, base: &str) -> Vec<NavPoint> {
    let mut out = Vec::new();
    for li in ol.children().filter(|n| n.has_tag_name("li")) {
        let Some(a) = li.children().find(|n| n.has_tag_name("a")) else {
            continue;
        };
        let Some(href) = a.attribute("href") else {
            continue;
        };
        let label = a
            .descendants()
            .filter_map(|n| n.text())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let children = li
            .children()
            .find(|n| n.has_tag_name("ol"))
            .map(|sub| parse_nav_ol(sub, base))
            .unwrap_or_default();
        out.push(NavPoint {
            label,
            doc: resolve(base, href),
            anchor: href.split_once('#').map(|(_, a)| a.to_string()),
            children,
        });
    }
    out
}
