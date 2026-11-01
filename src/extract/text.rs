//! Markdown / plain-text reader. `#` lines are headings, blank lines separate
//! paragraphs, and an optional YAML front-matter block supplies metadata.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;

use super::{non_empty, split_authors};
use crate::model::{Block, BlockKind, BookMetadata, RawDocument};

static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.*?)\s*#*\s*$").unwrap());
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[-*+]\s+(.*)$").unwrap());
static NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d+)[.)]\s+(.*)$").unwrap());
static SCENE_BREAK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:\*\s*){3,}|(?:-\s*){3,}|(?:_\s*){3,}|⁂|#)\s*$").unwrap()
});
static FOOTNOTE_DEF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[\^[^\]]+\]:").unwrap());
static FOOTNOTE_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\^[^\]]+\]").unwrap());
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").unwrap());

pub fn extract(path: &Path) -> Result<RawDocument> {
    let raw = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&raw);
    let mut doc = parse(text.trim_start_matches('\u{FEFF}'));
    doc.metadata.source_format = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt")
        .to_ascii_lowercase();
    Ok(doc)
}

pub fn parse(text: &str) -> RawDocument {
    let mut lines: Vec<&str> = text.lines().collect();
    let metadata = take_front_matter(&mut lines);

    let mut blocks = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut in_footnote = false;

    let flush = |para: &mut Vec<String>, blocks: &mut Vec<Block>| {
        if !para.is_empty() {
            blocks.push(Block::paragraph(inline(&para.join(" "))));
            para.clear();
        }
    };

    for line in lines {
        let trimmed = line.trim();
        if FOOTNOTE_DEF.is_match(trimmed) {
            flush(&mut para, &mut blocks);
            in_footnote = true;
            continue;
        }
        if in_footnote {
            // Footnote definitions continue on indented lines.
            if trimmed.is_empty() || line.starts_with("    ") || line.starts_with('\t') {
                continue;
            }
            in_footnote = false;
        }
        if trimmed.is_empty() {
            flush(&mut para, &mut blocks);
        } else if let Some(c) = HEADING.captures(trimmed) {
            flush(&mut para, &mut blocks);
            blocks.push(Block::heading(c[1].len() as u8, inline(&c[2])));
        } else if SCENE_BREAK.is_match(trimmed) {
            flush(&mut para, &mut blocks);
            blocks.push(Block::scene_break());
        } else if let Some(c) = BULLET.captures(line) {
            flush(&mut para, &mut blocks);
            blocks.push(Block::new(
                BlockKind::ListItem {
                    ordered: false,
                    index: 0,
                },
                inline(&c[1]),
            ));
        } else if let Some(c) = NUMBERED.captures(line) {
            flush(&mut para, &mut blocks);
            let index = c[1].parse().unwrap_or(0);
            blocks.push(Block::new(
                BlockKind::ListItem {
                    ordered: true,
                    index,
                },
                inline(&c[2]),
            ));
        } else if matches!(blocks.last(), Some(b) if matches!(b.kind, BlockKind::ListItem { .. }))
            && para.is_empty()
            && line.starts_with([' ', '\t'])
        {
            // Continuation line of a list item.
            let last = blocks.last_mut().unwrap();
            last.text = format!("{} {}", last.text, inline(trimmed));
        } else {
            para.push(trimmed.to_string());
        }
    }
    flush(&mut para, &mut blocks);

    RawDocument {
        metadata,
        blocks,
        toc: Vec::new(),
        cover: None,
    }
}

/// Strip inline markdown that would otherwise be read aloud.
fn inline(s: &str) -> String {
    let s = FOOTNOTE_REF.replace_all(s, "");
    LINK.replace_all(&s, "$1").into_owned()
}

/// Parse and remove a leading `---` YAML block (simple `key: value` pairs and
/// `- item` lists only).
fn take_front_matter(lines: &mut Vec<&str>) -> BookMetadata {
    let mut meta = BookMetadata::default();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return meta;
    }
    let Some(end) = lines
        .iter()
        .skip(1)
        .position(|l| matches!(l.trim(), "---" | "..."))
    else {
        return meta;
    };
    let mut current_key = String::new();
    for line in &lines[1..=end] {
        let unquote = |v: &str| v.trim().trim_matches(['"', '\'']).to_string();
        if let Some(item) = line.trim().strip_prefix("- ") {
            let v = unquote(item);
            match current_key.as_str() {
                "author" | "authors" | "creator" => meta.authors.push(v),
                "subject" | "subjects" | "keywords" | "tags" => meta.subjects.push(v),
                _ => {}
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        current_key = key.trim().to_ascii_lowercase();
        let value = unquote(value);
        if value.is_empty() {
            continue;
        }
        match current_key.as_str() {
            "title" => meta.title = non_empty(&value),
            "subtitle" => meta.subtitle = non_empty(&value),
            "author" | "authors" | "creator" => meta.authors.extend(split_authors(&value)),
            "lang" | "language" => meta.language = non_empty(&value),
            "publisher" => meta.publisher = non_empty(&value),
            "date" => meta.published = non_empty(&value),
            "identifier" | "isbn" => meta.identifier = non_empty(&value),
            "description" | "abstract" => meta.description = non_empty(&value),
            "subject" | "subjects" | "keywords" => {
                meta.subjects.extend(value.split(',').filter_map(non_empty));
            }
            _ => {}
        }
    }
    lines.drain(..=end + 1);
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_front_matter_headings_lists_and_footnotes() {
        let doc = parse(
            "---\ntitle: \"The Keeper\"\nauthor:\n  - Ingrid Solberg\nlang: en\n---\n\n# Part One\n\n## Chapter 1\n\nA line\ncontinues.[^1]\n\n- one\n- two\n\n1. first\n\n* * *\n\n[^1]: A note.\n    More note.\n\nSee [the site](https://x.org).\n",
        );
        assert_eq!(doc.metadata.title.as_deref(), Some("The Keeper"));
        assert_eq!(doc.metadata.authors, vec!["Ingrid Solberg"]);
        assert_eq!(doc.metadata.language.as_deref(), Some("en"));
        let kinds: Vec<(BlockKind, &str)> = doc
            .blocks
            .iter()
            .map(|b| (b.kind, b.text.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (BlockKind::Heading { level: 1 }, "Part One"),
                (BlockKind::Heading { level: 2 }, "Chapter 1"),
                (BlockKind::Paragraph, "A line continues."),
                (
                    BlockKind::ListItem {
                        ordered: false,
                        index: 0
                    },
                    "one"
                ),
                (
                    BlockKind::ListItem {
                        ordered: false,
                        index: 0
                    },
                    "two"
                ),
                (
                    BlockKind::ListItem {
                        ordered: true,
                        index: 1
                    },
                    "first"
                ),
                (BlockKind::SceneBreak, ""),
                (BlockKind::Paragraph, "See the site."),
            ]
        );
    }
}
