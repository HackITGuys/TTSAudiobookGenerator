//! Split a cleaned block stream into chapters.
//!
//! Strategy, in order of preference:
//! 1. The source's own table of contents (EPUB nav/NCX, PDF outlines).
//! 2. Heading blocks (styles, font sizes, markdown `#`).
//! 3. Paragraphs that look like chapter headings ("Chapter 3", "Prologue").
//!
//! In every case the split level is chosen so that "Part" containers do not
//! swallow their chapters, and title-only parts are merged into the chapter
//! that follows them.

use std::sync::LazyLock;

use regex::Regex;

use crate::model::{Block, BlockKind, Chapter, TocEntry, TocTarget};

static PART_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(part|book|volume|del|bok|bind)\b\s*\S*").unwrap());

static CHAPTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)^(
            (part|book|volume|del|bok|bind|chapter|kapittel|kapitel|chapitre|cap[ií]tulo)\s+[\p{L}\d][\p{L}\d\-]*\b.*
          | (prologue|epilogue|prolog|epilog|preface|foreword|forord|introduction|innledning|afterword|etterord|interlude|appendix)\b.*
          | (?-i:[IVXLCDM]+)\.?
        )$",
    )
    .unwrap()
});

/// Chapters with these titles are not narrated (unless `keep_all`).
const SKIP_TITLES: &[&str] = &[
    "contents",
    "table of contents",
    "innhold",
    "innholdsfortegnelse",
    "index",
    "register",
    "notes",
    "endnotes",
    "footnotes",
    "noter",
    "fotnoter",
    "list of figures",
    "list of tables",
];

/// Minimum words for text before the first chapter to become "Front Matter".
const FRONT_MATTER_MIN_WORDS: usize = 50;
/// A level is a "container" (e.g. Parts) if its entries have less than this
/// many words before their first child.
const CONTAINER_MAX_WORDS: usize = 40;
/// Paragraphs longer than this are never promoted to chapter headings.
const PROMOTE_MAX_CHARS: usize = 80;

#[derive(Debug, Clone)]
struct SplitPoint {
    idx: usize,
    level: u8,
    title: String,
}

pub struct SplitOptions<'a> {
    pub book_title: Option<&'a str>,
    pub keep_all: bool,
}

/// Normalised form for fuzzy title comparison: lower-case alphanumerics.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn body_words(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .filter(|b| !b.is_heading())
        .map(Block::word_count)
        .sum()
}

pub fn split_chapters(
    mut blocks: Vec<Block>,
    toc: &[TocEntry],
    opts: &SplitOptions,
) -> Vec<Chapter> {
    // A leading heading that repeats the book title is a title page, not a chapter.
    if let Some(title) = opts.book_title.map(norm).filter(|t| !t.is_empty()) {
        let lead = blocks
            .iter()
            .take(10)
            .position(|b| b.is_heading() && norm(&b.text) == title);
        if let Some(i) = lead {
            blocks.remove(i);
        }
    }

    let mut points = toc_points(&blocks, toc);
    if points.len() < 2 {
        points = heading_points(&blocks);
    }
    if points.len() < 2 {
        promote_chapter_paragraphs(&mut blocks);
        points = heading_points(&blocks);
    }

    if points.is_empty() {
        // No usable structure: narrate everything as a single chapter.
        let title = opts.book_title.unwrap_or("Full Text").to_string();
        return if blocks.is_empty() {
            Vec::new()
        } else {
            vec![Chapter::new(1, title, blocks)]
        };
    }

    let mut drafts = cut(blocks, &points);
    for d in &mut drafts {
        absorb_leading_heading(d);
    }
    let drafts = merge_and_filter(drafts, opts);

    drafts
        .into_iter()
        .enumerate()
        .map(|(i, d)| Chapter::new(i + 1, d.title, d.blocks))
        .collect()
}

// ---------------------------------------------------------------- TOC

fn toc_points(blocks: &[Block], toc: &[TocEntry]) -> Vec<SplitPoint> {
    let mut points: Vec<SplitPoint> = toc
        .iter()
        .filter_map(|e| {
            locate_toc_entry(blocks, e).map(|idx| SplitPoint {
                idx,
                level: e.level,
                title: e.title.trim().to_string(),
            })
        })
        .filter(|p| !p.title.is_empty())
        .collect();
    points.sort_by_key(|p| p.idx); // stable: keeps TOC order for shared targets
    select_levels(blocks, points)
}

fn locate_toc_entry(blocks: &[Block], entry: &TocEntry) -> Option<usize> {
    match &entry.target {
        TocTarget::Doc { doc, anchor } => {
            let in_doc = |b: &Block| b.doc == Some(*doc);
            if let Some(anchor) = anchor
                && let Some(i) = blocks
                    .iter()
                    .position(|b| b.anchors.iter().any(|a| a == anchor))
            {
                return Some(i);
            }
            blocks
                .iter()
                .position(in_doc)
                .or_else(|| blocks.iter().position(|b| b.doc.is_some_and(|d| d > *doc)))
        }
        TocTarget::Page(page) => {
            let first = blocks
                .iter()
                .position(|b| b.page.is_some_and(|p| p >= *page))?;
            let title = norm(&entry.title);
            let window = blocks[first..]
                .iter()
                .enumerate()
                .take_while(|(_, b)| b.page.is_some_and(|p| p <= page + 1));
            let matches = |b: &Block| {
                let t = norm(&b.text);
                !t.is_empty()
                    && !title.is_empty()
                    && (t.contains(&title) || (title.contains(&t) && t.len() > 3))
            };
            let mut heading_match = None;
            let mut any_match = None;
            for (off, b) in window {
                if matches(b) {
                    if b.is_heading() {
                        heading_match = Some(first + off);
                        break;
                    }
                    any_match.get_or_insert(first + off);
                }
            }
            Some(heading_match.or(any_match).unwrap_or(first))
        }
    }
}

// ---------------------------------------------------------------- headings

fn heading_points(blocks: &[Block]) -> Vec<SplitPoint> {
    let points = blocks
        .iter()
        .enumerate()
        .filter_map(|(idx, b)| {
            b.heading_level().map(|level| SplitPoint {
                idx,
                level,
                title: b.text.clone(),
            })
        })
        .collect();
    select_levels(blocks, points)
}

fn promote_chapter_paragraphs(blocks: &mut [Block]) {
    for b in blocks.iter_mut() {
        if b.kind == BlockKind::Paragraph
            && b.text.chars().count() <= PROMOTE_MAX_CHARS
            && CHAPTER_RE.is_match(b.text.trim())
        {
            let level = if PART_RE.is_match(&b.text) { 1 } else { 2 };
            b.kind = BlockKind::Heading { level };
        }
    }
}

/// Pick the deepest level that should still start a new chapter and drop
/// split points below it.
fn select_levels(blocks: &[Block], points: Vec<SplitPoint>) -> Vec<SplitPoint> {
    let mut levels: Vec<u8> = points.iter().map(|p| p.level).collect();
    levels.sort_unstable();
    levels.dedup();

    let mut chosen = levels.last().copied().unwrap_or(1);
    for (i, &level) in levels.iter().enumerate() {
        let has_deeper = i + 1 < levels.len();
        let at_level: Vec<&SplitPoint> = points.iter().filter(|p| p.level == level).collect();
        if has_deeper
            && (at_level.len() < 2
                || mostly_parts(&at_level)
                || is_container(blocks, &points, level))
        {
            continue;
        }
        chosen = level;
        break;
    }
    points.into_iter().filter(|p| p.level <= chosen).collect()
}

fn mostly_parts(points: &[&SplitPoint]) -> bool {
    let parts = points.iter().filter(|p| PART_RE.is_match(&p.title)).count();
    parts * 2 >= points.len()
}

/// Entries at `level` that have children carry (almost) no text of their
/// own before the first child: they are containers such as Parts.
fn is_container(blocks: &[Block], points: &[SplitPoint], level: u8) -> bool {
    let mut with_children = 0;
    let mut empty = 0;
    for (i, p) in points.iter().enumerate() {
        if p.level != level {
            continue;
        }
        let Some(next) = points.get(i + 1) else {
            continue;
        };
        if next.level <= level {
            continue;
        }
        with_children += 1;
        let end = next.idx.max(p.idx);
        if body_words(&blocks[p.idx..end]) < CONTAINER_MAX_WORDS {
            empty += 1;
        }
    }
    with_children > 0 && empty * 3 >= with_children * 2
}

// ---------------------------------------------------------------- cutting

#[derive(Debug)]
struct Draft {
    title: String,
    blocks: Vec<Block>,
    /// Content before the first split point.
    front: bool,
}

fn cut(blocks: Vec<Block>, points: &[SplitPoint]) -> Vec<Draft> {
    let mut drafts = Vec::new();
    let mut iter = blocks.into_iter().enumerate().peekable();

    let first_idx = points.first().map_or(usize::MAX, |p| p.idx);
    let front: Vec<Block> =
        std::iter::from_fn(|| iter.next_if(|(i, _)| *i < first_idx).map(|(_, b)| b)).collect();
    if !front.is_empty() {
        drafts.push(Draft {
            title: "Front Matter".into(),
            blocks: front,
            front: true,
        });
    }

    for (n, p) in points.iter().enumerate() {
        let end = points.get(n + 1).map_or(usize::MAX, |q| q.idx);
        let blocks: Vec<Block> =
            std::iter::from_fn(|| iter.next_if(|(i, _)| *i < end).map(|(_, b)| b)).collect();
        drafts.push(Draft {
            title: p.title.clone(),
            blocks,
            front: false,
        });
    }
    drafts
}

/// If a chapter starts with its own heading, fold it into the title so the
/// title is not read twice. The more complete of the two texts wins.
fn absorb_leading_heading(d: &mut Draft) {
    let Some(first) = d.blocks.first() else {
        return;
    };
    if !first.is_heading() {
        return;
    }
    let (h, t) = (norm(&first.text), norm(&d.title));
    if h.is_empty() || !(h.contains(&t) || t.contains(&h)) {
        return;
    }
    if h.len() > t.len() {
        d.title = first.text.clone();
    }
    d.blocks.remove(0);
}

fn merge_and_filter(drafts: Vec<Draft>, opts: &SplitOptions) -> Vec<Draft> {
    let book_title = opts.book_title.map(norm);
    let mut out: Vec<Draft> = Vec::new();
    let mut carry_title: Option<String> = None;
    let mut carry_blocks: Vec<Block> = Vec::new();

    for mut d in drafts {
        let words = body_words(&d.blocks);
        let skip_title = SKIP_TITLES.contains(&norm(&d.title).as_str());

        if d.front && words < FRONT_MATTER_MIN_WORDS {
            continue;
        }
        if !opts.keep_all && skip_title {
            continue;
        }
        if words < FRONT_MATTER_MIN_WORDS && book_title.as_deref() == Some(norm(&d.title).as_str())
        {
            continue; // title page
        }
        if words == 0 {
            // Title-only entry: a Part or a "Chapter 1" above its subtitle is
            // prefixed to the next chapter; anything else (cover, image
            // pages) is dropped.
            if CHAPTER_RE.is_match(d.title.trim()) || PART_RE.is_match(&d.title) {
                carry_title = Some(match carry_title.take() {
                    Some(prev) => format!("{prev} — {}", d.title),
                    None => d.title,
                });
                carry_blocks.append(&mut d.blocks);
            }
            continue;
        }
        if let Some(prefix) = carry_title.take() {
            d.title = format!("{prefix} — {}", d.title);
            carry_blocks.append(&mut d.blocks);
            d.blocks = std::mem::take(&mut carry_blocks);
        }
        out.push(d);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(words: usize) -> Block {
        Block::paragraph(vec!["word"; words].join(" "))
    }

    fn opts() -> SplitOptions<'static> {
        SplitOptions {
            book_title: Some("The Keeper"),
            keep_all: false,
        }
    }

    fn titles(chapters: &[Chapter]) -> Vec<&str> {
        chapters.iter().map(|c| c.title.as_str()).collect()
    }

    #[test]
    fn splits_on_headings_and_merges_parts() {
        let blocks = vec![
            Block::heading(1, "The Keeper"),
            para(10),
            Block::heading(1, "Part One"),
            Block::heading(2, "Chapter 1: Fog"),
            para(100),
            Block::heading(3, "A Section"),
            para(100),
            Block::heading(2, "Chapter 2: Light"),
            para(100),
            Block::heading(1, "Part Two"),
            Block::heading(2, "Chapter 3: Home"),
            para(100),
        ];
        let chapters = split_chapters(blocks, &[], &opts());
        assert_eq!(
            titles(&chapters),
            vec![
                "Part One — Chapter 1: Fog",
                "Chapter 2: Light",
                "Part Two — Chapter 3: Home"
            ]
        );
        // The section heading stays inside chapter 1.
        assert!(chapters[0].blocks.iter().any(|b| b.text == "A Section"));
        assert_eq!(chapters[0].index, 1);
    }

    #[test]
    fn drops_contents_and_short_front_matter() {
        let blocks = vec![
            para(5),
            Block::heading(1, "Contents"),
            Block::paragraph("Chapter 1"),
            Block::heading(1, "Chapter 1"),
            para(50),
            Block::heading(1, "Chapter 2"),
            para(50),
        ];
        let chapters = split_chapters(blocks, &[], &opts());
        assert_eq!(titles(&chapters), vec!["Chapter 1", "Chapter 2"]);
    }

    #[test]
    fn promotes_chapter_like_paragraphs() {
        let blocks = vec![
            Block::paragraph("Prologue"),
            para(30),
            Block::paragraph("CHAPTER ONE"),
            para(30),
            Block::paragraph("Chapter Two"),
            para(30),
        ];
        let chapters = split_chapters(blocks, &[], &opts());
        assert_eq!(
            titles(&chapters),
            vec!["Prologue", "CHAPTER ONE", "Chapter Two"]
        );
    }

    #[test]
    fn uses_toc_with_epub_anchors() {
        let mut blocks = vec![
            para(5),
            Block::heading(1, "Fog"),
            para(60),
            Block::heading(1, "Light"),
            para(60),
        ];
        for b in &mut blocks {
            b.doc = Some(1);
        }
        blocks[0].doc = Some(0);
        blocks[3].anchors = vec!["light".into()];
        let toc = vec![
            TocEntry {
                title: "Chapter 1. Fog".into(),
                level: 1,
                target: TocTarget::Doc {
                    doc: 1,
                    anchor: None,
                },
            },
            TocEntry {
                title: "Light".into(),
                level: 1,
                target: TocTarget::Doc {
                    doc: 1,
                    anchor: Some("light".into()),
                },
            },
        ];
        let chapters = split_chapters(blocks, &toc, &opts());
        assert_eq!(titles(&chapters), vec!["Chapter 1. Fog", "Light"]);
        assert!(chapters.iter().all(|c| c.blocks.len() == 1));
    }

    #[test]
    fn uses_pdf_outline_pages() {
        let mut blocks = vec![
            Block::heading(1, "Chapter 1: Fog"),
            para(60),
            para(60),
            Block::heading(1, "Chapter 2: Light"),
            para(60),
        ];
        for (b, page) in blocks.iter_mut().zip([3, 3, 4, 4, 4]) {
            b.page = Some(page);
        }
        let toc = vec![
            TocEntry {
                title: "Fog".into(),
                level: 1,
                target: TocTarget::Page(3),
            },
            TocEntry {
                title: "Light".into(),
                level: 1,
                target: TocTarget::Page(4),
            },
        ];
        let chapters = split_chapters(blocks, &toc, &opts());
        // Titles come from the more complete heading text.
        assert_eq!(
            titles(&chapters),
            vec!["Chapter 1: Fog", "Chapter 2: Light"]
        );
        assert_eq!(chapters[0].blocks.len(), 2);
    }

    #[test]
    fn falls_back_to_single_chapter() {
        let chapters = split_chapters(vec![para(20), para(20)], &[], &opts());
        assert_eq!(titles(&chapters), vec!["The Keeper"]);
        assert_eq!(chapters[0].blocks.len(), 2);
    }
}
