//! EPUB 2/3 reader: OPF metadata, spine order, nav/NCX table of contents and
//! cover image.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use roxmltree::{Document, Node};
use scraper::{ElementRef, Html, Selector};

use super::{
    collapse_ws, html::xhtml_to_blocks, non_empty, parse_xml, read_zip_bytes, read_zip_text,
};
use crate::model::{BookMetadata, Cover, RawDocument, TocEntry, TocTarget};

struct ManifestItem {
    href: String,
    media_type: String,
    properties: String,
}

pub fn extract(path: &Path) -> Result<RawDocument> {
    let mut zip = zip::ZipArchive::new(File::open(path)?)?;

    let container = read_zip_text(&mut zip, "META-INF/container.xml")?;
    let container = parse_xml(&container)?;
    let opf_path = container
        .descendants()
        .find(|n| n.has_tag_name("rootfile"))
        .and_then(|n| n.attribute("full-path"))
        .context("container.xml has no rootfile")?
        .to_string();
    let opf_text = read_zip_text(&mut zip, &opf_path)?;
    let opf = parse_xml(&opf_text).context("invalid OPF package document")?;
    let opf_dir = parent_dir(&opf_path);

    let mut metadata = read_metadata(&opf);
    metadata.source_format = "epub".into();

    let mut manifest: HashMap<String, ManifestItem> = HashMap::new();
    for item in opf.descendants().filter(|n| n.has_tag_name("item")) {
        let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) else {
            continue;
        };
        manifest.insert(
            id.to_string(),
            ManifestItem {
                href: resolve(&opf_dir, href),
                media_type: item.attribute("media-type").unwrap_or("").to_string(),
                properties: item.attribute("properties").unwrap_or("").to_string(),
            },
        );
    }

    let spine_node = opf.descendants().find(|n| n.has_tag_name("spine"));
    let spine: Vec<&ManifestItem> = spine_node
        .iter()
        .flat_map(|s| s.children().filter(|n| n.has_tag_name("itemref")))
        .filter(|r| r.attribute("linear") != Some("no"))
        .filter_map(|r| manifest.get(r.attribute("idref")?))
        .filter(|m| {
            m.media_type.contains("html") || m.href.ends_with("html") || m.href.ends_with(".htm")
        })
        .collect();
    let spine_index: HashMap<&str, usize> = spine
        .iter()
        .enumerate()
        .map(|(i, m)| (m.href.as_str(), i))
        .collect();

    let mut blocks = Vec::new();
    for (i, item) in spine.iter().enumerate() {
        let xhtml = read_zip_text(&mut zip, &item.href)?;
        blocks.extend(xhtml_to_blocks(&xhtml, Some(i)));
    }

    // EPUB 3 navigation document first, then the EPUB 2 NCX.
    let nav = manifest
        .values()
        .find(|m| m.properties.split_whitespace().any(|p| p == "nav"));
    let mut toc = match nav {
        Some(nav) => read_zip_text(&mut zip, &nav.href)
            .map(|t| parse_nav(&t, &parent_dir(&nav.href), &spine_index))
            .unwrap_or_default(),
        None => Vec::new(),
    };
    if toc.is_empty() {
        let ncx = spine_node
            .and_then(|s| s.attribute("toc"))
            .and_then(|id| manifest.get(id))
            .or_else(|| {
                manifest
                    .values()
                    .find(|m| m.media_type == "application/x-dtbncx+xml")
            });
        if let Some(ncx) = ncx
            && let Ok(t) = read_zip_text(&mut zip, &ncx.href)
        {
            toc = parse_ncx(&t, &parent_dir(&ncx.href), &spine_index);
        }
    }

    let cover = find_cover(&opf, &manifest).and_then(|item| {
        let bytes = read_zip_bytes(&mut zip, &item.href).ok()?;
        let extension = match item.media_type.as_str() {
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "jpg",
        };
        Some(Cover {
            bytes,
            extension: extension.into(),
        })
    });

    Ok(RawDocument {
        metadata,
        blocks,
        toc,
        cover,
    })
}

fn read_metadata(opf: &Document) -> BookMetadata {
    let mut meta = BookMetadata::default();
    let Some(md) = opf.descendants().find(|n| n.has_tag_name("metadata")) else {
        return meta;
    };

    // EPUB 3 refinements: <meta refines="#id" property="role">aut</meta>
    let mut refines: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for m in md.children().filter(|n| n.has_tag_name("meta")) {
        if let (Some(target), Some(prop)) = (m.attribute("refines"), m.attribute("property")) {
            refines
                .entry(target.trim_start_matches('#').to_string())
                .or_default()
                .push((prop.to_string(), collapse_ws(m.text().unwrap_or(""))));
        }
    }
    let refined = |n: &Node, prop: &str| -> Option<String> {
        let id = n.attribute("id")?;
        refines
            .get(id)?
            .iter()
            .find(|(p, _)| p == prop)
            .map(|(_, v)| v.clone())
    };

    let mut titles: Vec<(String, Option<String>)> = Vec::new();
    for n in md.children().filter(Node::is_element) {
        let text = collapse_ws(n.text().unwrap_or(""));
        if text.is_empty() {
            continue;
        }
        match n.tag_name().name() {
            "title" => titles.push((text, refined(&n, "title-type"))),
            "creator" => {
                let role = n
                    .attributes()
                    .find(|a| a.name() == "role")
                    .map(|a| a.value().to_string())
                    .or_else(|| refined(&n, "role"));
                if role.as_deref().is_none_or(|r| r == "aut") {
                    meta.authors.push(text);
                }
            }
            "language" => meta.language = meta.language.take().or(Some(text)),
            "publisher" => meta.publisher = meta.publisher.take().or(Some(text)),
            "date" => meta.published = meta.published.take().or(Some(text)),
            "description" => meta.description = non_empty(strip_tags(&text)),
            "subject" => meta.subjects.push(text),
            "identifier" => {
                let is_isbn = text.to_ascii_lowercase().contains("isbn")
                    || n.attributes()
                        .any(|a| a.name() == "scheme" && a.value().eq_ignore_ascii_case("isbn"));
                if is_isbn || meta.identifier.is_none() {
                    meta.identifier = Some(text.trim_start_matches("urn:isbn:").to_string());
                }
            }
            _ => {}
        }
    }

    let main = titles
        .iter()
        .position(|(_, t)| t.as_deref() == Some("main"))
        .unwrap_or(0);
    if let Some((t, _)) = titles.get(main) {
        meta.title = Some(t.clone());
    }
    meta.subtitle = titles
        .iter()
        .find(|(_, t)| t.as_deref() == Some("subtitle"))
        .map(|(s, _)| s.clone());
    meta
}

fn strip_tags(s: &str) -> String {
    let frag = Html::parse_fragment(s);
    collapse_ws(&frag.root_element().text().collect::<String>())
}

fn find_cover<'m>(
    opf: &Document,
    manifest: &'m HashMap<String, ManifestItem>,
) -> Option<&'m ManifestItem> {
    manifest
        .values()
        .find(|m| m.properties.split_whitespace().any(|p| p == "cover-image"))
        .or_else(|| {
            let id = opf
                .descendants()
                .find(|n| n.has_tag_name("meta") && n.attribute("name") == Some("cover"))?
                .attribute("content")?;
            manifest
                .get(id)
                .filter(|m| m.media_type.starts_with("image/"))
        })
}

// ---------------------------------------------------------------- TOC

fn target(href: &str, base_dir: &str, spine_index: &HashMap<&str, usize>) -> Option<TocTarget> {
    let (file, fragment) = match href.split_once('#') {
        Some((f, frag)) => (f, Some(frag)),
        None => (href, None),
    };
    let path = if file.is_empty() {
        return None;
    } else {
        resolve(base_dir, file)
    };
    let doc = *spine_index.get(path.as_str())?;
    Some(TocTarget::Doc {
        doc,
        anchor: fragment.filter(|f| !f.is_empty()).map(percent_decode),
    })
}

fn parse_nav(xhtml: &str, base_dir: &str, spine_index: &HashMap<&str, usize>) -> Vec<TocEntry> {
    let html = Html::parse_document(xhtml);
    let navs = Selector::parse("nav").unwrap();
    let toc_nav = html
        .select(&navs)
        .find(|n| {
            n.value().attrs().any(|(k, v)| {
                matches!(k, "epub:type" | "type" | "role")
                    && v.split_whitespace().any(|t| t == "toc" || t == "doc-toc")
            })
        })
        .or_else(|| html.select(&navs).next());
    let Some(nav) = toc_nav else {
        return Vec::new();
    };
    let Some(ol) = nav
        .children()
        .filter_map(ElementRef::wrap)
        .find(|c| c.value().name() == "ol")
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_nav_list(ol, 1, base_dir, spine_index, &mut out);
    out
}

fn walk_nav_list(
    ol: ElementRef,
    level: u8,
    base_dir: &str,
    spine_index: &HashMap<&str, usize>,
    out: &mut Vec<TocEntry>,
) {
    for li in ol
        .children()
        .filter_map(ElementRef::wrap)
        .filter(|c| c.value().name() == "li")
    {
        let children: Vec<ElementRef> = li.children().filter_map(ElementRef::wrap).collect();
        if let Some(a) = children.iter().find(|c| c.value().name() == "a") {
            let title = collapse_ws(&a.text().collect::<String>());
            if let Some(t) = a
                .value()
                .attr("href")
                .and_then(|h| target(h, base_dir, spine_index))
            {
                out.push(TocEntry {
                    title,
                    level,
                    target: t,
                });
            }
        }
        for sub in children.iter().filter(|c| c.value().name() == "ol") {
            walk_nav_list(*sub, level.saturating_add(1), base_dir, spine_index, out);
        }
    }
}

fn parse_ncx(xml: &str, base_dir: &str, spine_index: &HashMap<&str, usize>) -> Vec<TocEntry> {
    let Ok(doc) = parse_xml(xml) else {
        return Vec::new();
    };
    let Some(nav_map) = doc.descendants().find(|n| n.has_tag_name("navMap")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_nav_points(nav_map, 1, base_dir, spine_index, &mut out);
    out
}

fn walk_nav_points(
    parent: Node,
    level: u8,
    base_dir: &str,
    spine_index: &HashMap<&str, usize>,
    out: &mut Vec<TocEntry>,
) {
    for np in parent
        .children()
        .filter(|n| n.tag_name().name() == "navPoint")
    {
        let title = np
            .descendants()
            .find(|n| n.tag_name().name() == "navLabel")
            .map(|l| collapse_ws(&l.descendants().filter_map(|t| t.text()).collect::<String>()))
            .unwrap_or_default();
        let src = np
            .children()
            .find(|n| n.tag_name().name() == "content")
            .and_then(|c| c.attribute("src"));
        if let Some(t) = src.and_then(|s| target(s, base_dir, spine_index)) {
            out.push(TocEntry {
                title,
                level,
                target: t,
            });
        }
        walk_nav_points(np, level.saturating_add(1), base_dir, spine_index, out);
    }
}

// ---------------------------------------------------------------- paths

fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default()
}

/// Resolve a (percent-encoded) relative href against a zip directory.
fn resolve(base_dir: &str, href: &str) -> String {
    let href = percent_decode(href);
    let mut parts: Vec<&str> = if href.starts_with('/') {
        Vec::new()
    } else {
        base_dir.split('/').filter(|p| !p.is_empty()).collect()
    };
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_hrefs() {
        assert_eq!(
            resolve("OEBPS/Text", "../Images/c%20over.jpg"),
            "OEBPS/Images/c over.jpg"
        );
        assert_eq!(resolve("", "text/ch1.xhtml"), "text/ch1.xhtml");
        assert_eq!(resolve("OEBPS", "./ch1.xhtml"), "OEBPS/ch1.xhtml");
    }

    #[test]
    fn parses_nested_nav() {
        let nav = r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body>
            <nav epub:type="landmarks"><ol><li><a href="ch1.xhtml">Start</a></li></ol></nav>
            <nav epub:type="toc"><ol>
              <li><a href="part1.xhtml">Part One</a>
                <ol><li><a href="ch1.xhtml#c1">Chapter 1</a></li></ol></li>
              <li><span>Unlinked</span></li>
            </ol></nav></body></html>"#;
        let spine: HashMap<&str, usize> = [("text/part1.xhtml", 0), ("text/ch1.xhtml", 1)].into();
        let toc = parse_nav(nav, "text", &spine);
        assert_eq!(toc.len(), 2);
        assert_eq!(toc[0].title, "Part One");
        assert_eq!(toc[1].level, 2);
        assert_eq!(
            toc[1].target,
            TocTarget::Doc {
                doc: 1,
                anchor: Some("c1".into())
            }
        );
    }
}
