//! PDF reader.
//!
//! PDFs carry no paragraph or heading markup, so structure is rebuilt from
//! glyph geometry: characters become lines, lines are classified by font size
//! (headings are larger than the body text), running headers/footers, page
//! numbers and footnotes are removed, and lines are joined into paragraphs
//! using vertical gaps and first-line indents.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use pdf_extract::{ColorSpace, MediaBox, OutputDev, OutputError, Path, PathOp, Transform};
use regex::Regex;

use super::{is_scene_break_text, non_empty, split_authors};
use crate::model::{Block, BlockKind, BookMetadata, RawDocument, TocEntry, TocTarget};

/// Lines at least this much larger than the body text are headings.
const HEADING_RATIO: f64 = 1.15;
/// Lines this much smaller than the body text at the page bottom may be footnotes.
const FOOTNOTE_RATIO: f64 = 0.92;
/// A horizontal gap wider than this many ems separates table cells.
const CELL_GAP: f64 = 1.0;
/// Safety stop for a table region that never meets prose again.
const MAX_TABLE_LINES: usize = 400;

static PAGE_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[\s\-–—]*(?:page\s+|side\s+)?(?:\d{1,4}|[ivxlcdm]{1,7})(?:\s+(?:of|av)\s+\d+)?[\s\-–—]*$").unwrap()
});
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[•●▪■◦‣⁃∙·–\-]\s*(.+)$").unwrap());
static NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,3})[.)]\s+(.+)$").unwrap());
static TABLE_CAPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Table\s+[A-Z]?\d+[-.–]\d+\s*[:.]\s*\S").unwrap());
static FIGURE_CAPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Figure\s+[A-Z]?\d+[-.–]\d+\s*[:.]\s*\S").unwrap());
/// "C H A P T E R" and other letter-spaced display words.
static LETTER_SPACED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\p{L} ){3,}\p{L}\b").unwrap());
/// "CHAPTER: 1: Title" / "Chapter 1 Title" -> "Chapter 1: Title".
static NUMBERED_HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?i:(chapter|part|appendix))\s*:?\s*(\d+\b|[IVXLCDM]+\b|[A-Z]\b)\s*:?\s*")
        .unwrap()
});

#[derive(Debug, Clone)]
struct Glyph {
    x: f64,
    y: f64,
    end: f64,
    size: f64,
    text: String,
}

#[derive(Debug, Clone)]
struct Line {
    text: String,
    x0: f64,
    x1: f64,
    /// Baseline, measured from the top of the page.
    y: f64,
    size: f64,
    /// The line starts with a digit or note symbol (before superscript removal).
    lead_mark: bool,
    /// Runs of text separated by wide gaps (table columns), with their x.
    cells: Vec<(f64, String)>,
}

#[derive(Debug, Default)]
struct Page {
    num: u32,
    height: f64,
    lines: Vec<Line>,
    /// Baselines (from the top) of centred horizontal rules: scene breaks.
    rules: Vec<f64>,
}

#[derive(Default)]
struct Collector {
    pages: Vec<Page>,
    flip: Transform,
    width: f64,
    glyphs: Vec<Glyph>,
    current: Page,
}

impl Collector {
    /// Record thin, wide, horizontally centred shapes. LaTeX and most layout
    /// tools draw scene-break rules this way; footnote rules are left-aligned.
    fn note_rule(&mut self, ctm: &Transform, path: &Path) {
        let m = ctm.post_transform(&self.flip);
        let mut pts = Vec::new();
        for op in &path.ops {
            match *op {
                PathOp::MoveTo(x, y) | PathOp::LineTo(x, y) => pts.push((x, y)),
                PathOp::Rect(x, y, w, h) => pts.extend([(x, y), (x + w, y + h)]),
                PathOp::CurveTo(..) => return,
                PathOp::Close => {}
            }
        }
        let pts: Vec<_> = pts
            .into_iter()
            .map(|(x, y)| m.transform_point((x, y).into()))
            .collect();
        if pts.is_empty() || self.width <= 0.0 {
            return;
        }
        let (x0, x1) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.x), b.max(p.x)));
        let (y0, y1) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.y), b.max(p.y)));
        let centred = ((x0 + x1) / 2.0 - self.width / 2.0).abs() < self.width * 0.1;
        if y1 - y0 < 2.0 && x1 - x0 > self.width * 0.15 && centred {
            self.current.rules.push((y0 + y1) / 2.0);
        }
    }
}

impl OutputDev for Collector {
    fn begin_page(
        &mut self,
        page_num: u32,
        media_box: &MediaBox,
        _: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        let height = media_box.ury - media_box.lly;
        self.flip = Transform::row_major(1., 0., 0., -1., 0., height);
        self.width = media_box.urx - media_box.llx;
        self.glyphs.clear();
        self.current = Page {
            num: page_num,
            height,
            ..Default::default()
        };
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), OutputError> {
        let mut page = std::mem::take(&mut self.current);
        page.lines = build_lines(std::mem::take(&mut self.glyphs));
        page.rules.sort_by(f64::total_cmp);
        self.pages.push(page);
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &Transform,
        width: f64,
        _spacing: f64,
        font_size: f64,
        char: &str,
    ) -> Result<(), OutputError> {
        let pos = trm.post_transform(&self.flip);
        let v = trm.transform_vector((font_size, font_size).into());
        let size = (v.x * v.y).abs().sqrt();
        if !size.is_finite() || size <= 0.0 {
            return Ok(());
        }
        self.glyphs.push(Glyph {
            x: pos.m31,
            y: pos.m32,
            end: pos.m31 + width * size,
            size,
            text: char.to_string(),
        });
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn fill(
        &mut self,
        ctm: &Transform,
        _: &ColorSpace,
        _: &[f64],
        path: &Path,
    ) -> Result<(), OutputError> {
        self.note_rule(ctm, path);
        Ok(())
    }
    fn stroke(
        &mut self,
        ctm: &Transform,
        _: &ColorSpace,
        _: &[f64],
        path: &Path,
    ) -> Result<(), OutputError> {
        self.note_rule(ctm, path);
        Ok(())
    }
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    values[values.len() / 2]
}

/// Group glyphs (in content-stream order) into text lines.
fn build_lines(glyphs: Vec<Glyph>) -> Vec<Line> {
    let mut groups: Vec<Vec<Glyph>> = Vec::new();
    for g in glyphs {
        let is_space = g.text.trim().is_empty();
        match groups.last_mut() {
            Some(cur) if !cur.is_empty() => {
                let anchor = cur
                    .iter()
                    .find(|c| !c.text.trim().is_empty())
                    .unwrap_or(&cur[0]);
                let same_line = (g.y - anchor.y).abs() <= 0.5 * anchor.size.max(g.size);
                if same_line || is_space {
                    cur.push(g);
                } else {
                    groups.push(vec![g]);
                }
            }
            _ => groups.push(vec![g]),
        }
    }
    groups.into_iter().filter_map(render_line).collect()
}

fn is_ligature(text: &str) -> bool {
    matches!(
        text,
        "fi" | "fl" | "ff" | "ffi" | "ffl" | "ft" | "st" | "ﬁ" | "ﬂ" | "ﬀ" | "ﬃ" | "ﬄ" | "ﬅ" | "ﬆ"
    )
}

/// Some PDFs (e.g. Wiley's) emit a space glyph that overlaps a ligature
/// glyph ("fi", "fl"); taken at face value it splits the word ("spoofi ng").
/// A real word space starts after the previous glyph and ends before the
/// next one.
fn phantom_spaces(glyphs: &[Glyph]) -> Vec<bool> {
    let visible = |g: &&Glyph| !g.text.trim().is_empty();
    (0..glyphs.len())
        .map(|i| {
            let g = &glyphs[i];
            if !g.text.trim().is_empty() {
                return false;
            }
            let tol = 0.01 * g.size;
            let prev = glyphs[..i].iter().rev().find(visible);
            let next = glyphs[i + 1..].iter().find(visible);
            prev.is_some_and(|p| is_ligature(&p.text) && g.x < p.end - tol)
                || next.is_some_and(|n| is_ligature(&n.text) && g.end > n.x + tol)
        })
        .collect()
}

fn render_line(glyphs: Vec<Glyph>) -> Option<Line> {
    let phantom = phantom_spaces(&glyphs);
    let visible: Vec<&Glyph> = glyphs
        .iter()
        .filter(|g| !g.text.trim().is_empty())
        .collect();
    let first = *visible.first()?;
    let mut sizes: Vec<f64> = visible.iter().map(|g| g.size).collect();
    let size = median(&mut sizes);
    let baseline = first.y;
    let lead_mark = first
        .text
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || "*†‡§".contains(c));

    let mut text = String::new();
    let mut prev_end: Option<f64> = None;
    let mut x0 = f64::MAX;
    let mut x1 = f64::MIN;
    let mut cells: Vec<(f64, String)> = Vec::new();
    let mut prev_visible_end: Option<f64> = None;
    // The glyph before a phantom space often reports a short width too
    // ("f" + "fi" for "ffi"), so its gap is not a word break either.
    let mut after_phantom = false;
    for (i, g) in glyphs.iter().enumerate() {
        if g.text.trim().is_empty() {
            if phantom[i] {
                after_phantom = true;
                continue;
            }
            if !text.ends_with(' ') && !text.is_empty() {
                text.push(' ');
            }
            prev_end = Some(g.end);
            continue;
        }
        // Raised, smaller digits/symbols are footnote markers.
        let raised = baseline - g.y > 0.15 * size;
        if g.size < 0.8 * size
            && raised
            && g.text
                .chars()
                .all(|c| c.is_ascii_digit() || "*†‡§,".contains(c))
        {
            continue;
        }
        if let Some(pe) = prev_end
            && g.x > pe + 0.15 * g.size
            && !after_phantom
            && !text.ends_with(' ')
            && !text.is_empty()
        {
            text.push(' ');
        }
        after_phantom = false;
        let wide_gap = prev_visible_end.is_none_or(|e| g.x - e > CELL_GAP * g.size);
        match cells.last_mut() {
            Some(cell) if !wide_gap => {
                if text.ends_with(' ') && !cell.1.ends_with(' ') {
                    cell.1.push(' ');
                }
                cell.1.push_str(&g.text);
            }
            _ => cells.push((g.x, g.text.clone())),
        }
        text.push_str(&g.text);
        prev_end = Some(g.end);
        prev_visible_end = Some(g.end);
        x0 = x0.min(g.x);
        x1 = x1.max(g.end);
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(Line {
        text,
        x0,
        x1,
        y: baseline,
        size,
        lead_mark,
        cells: cells
            .into_iter()
            .map(|(x, t)| (x, t.trim().to_string()))
            .collect(),
    })
}

pub fn extract(path: &std::path::Path) -> Result<RawDocument> {
    let mut doc = pdf_extract::Document::load(path).context("cannot parse PDF")?;
    if doc.is_encrypted() {
        // Many "encrypted" PDFs only restrict printing and open with an empty password.
        doc.decrypt("").context("PDF is password protected")?;
    }

    let mut collector = Collector::default();
    let page_numbers: Vec<u32> = doc.get_pages().keys().copied().collect();
    for num in page_numbers {
        let result = catch_unwind(AssertUnwindSafe(|| {
            pdf_extract::output_doc_page(&doc, &mut collector, num)
        }));
        let ok = matches!(result, Ok(Ok(())));
        if !ok {
            eprintln!("warning: could not extract text from page {num}; skipping it");
            collector.glyphs.clear();
        }
    }

    let mut metadata = read_info(&doc);
    metadata.source_format = "pdf".into();
    let toc = read_outline(&doc);

    Ok(RawDocument {
        metadata,
        blocks: build_blocks(collector.pages),
        toc,
        cover: None,
    })
}

// ---------------------------------------------------------------- layout

fn size_key(size: f64) -> i64 {
    (size * 2.0).round() as i64
}

/// The most common font size, weighted by characters.
fn body_size(pages: &[Page]) -> f64 {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for l in pages.iter().flat_map(|p| &p.lines) {
        *counts.entry(size_key(l.size)).or_default() += l.text.chars().count();
    }
    counts
        .into_iter()
        .max_by_key(|(k, n)| (*n, -*k))
        .map_or(10.0, |(k, _)| k as f64 / 2.0)
}

fn normalise_furniture(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_digit() {
                '#'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Remove running headers/footers and page numbers: the first or last lines
/// of a page that are page numbers or repeat on other pages.
fn strip_page_furniture(pages: &mut [Page]) {
    const EDGE: usize = 2;
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in pages.iter() {
        let n = p.lines.len();
        for (i, l) in p.lines.iter().enumerate() {
            if i < EDGE || i + EDGE >= n {
                *seen.entry(normalise_furniture(&l.text)).or_default() += 1;
            }
        }
    }
    let is_furniture = |l: &Line| {
        PAGE_NUMBER.is_match(&l.text)
            // Running heads such as "12 Part I ■ Getting Started".
            || (l.text.contains(['■', '▪']) && l.text.chars().any(|c| c.is_ascii_digit()))
            || seen
                .get(&normalise_furniture(&l.text))
                .is_some_and(|c| *c >= 2)
    };

    for p in pages.iter_mut() {
        for _ in 0..EDGE {
            if p.lines.first().is_some_and(is_furniture) {
                p.lines.remove(0);
            }
        }
        for _ in 0..EDGE {
            if p.lines.last().is_some_and(is_furniture) {
                p.lines.pop();
            }
        }
    }
}

/// Remove footnotes: a run of small-type lines at the bottom of a page
/// starting with a note marker.
fn strip_footnotes(pages: &mut [Page], body: f64) {
    for p in pages.iter_mut() {
        let mut start = p.lines.len();
        while start > 0 {
            let l = &p.lines[start - 1];
            if l.size < body * FOOTNOTE_RATIO && l.y > p.height * 0.5 {
                start -= 1;
            } else {
                break;
            }
        }
        if let Some(first_mark) = (start..p.lines.len()).find(|&i| p.lines[i].lead_mark) {
            p.lines.truncate(first_mark);
        }
    }
}

#[derive(PartialEq)]
enum LineClass {
    Heading(u8),
    SceneBreak,
    ListItem {
        ordered: bool,
        index: u32,
        text: String,
    },
    Body,
}

fn build_blocks(mut pages: Vec<Page>) -> Vec<Block> {
    strip_page_furniture(&mut pages);
    let body = body_size(&pages);
    strip_footnotes(&mut pages, body);

    // Heading levels: distinct heading sizes, largest first.
    let mut heading_sizes: Vec<i64> = pages
        .iter()
        .flat_map(|p| &p.lines)
        .filter(|l| l.size >= body * HEADING_RATIO)
        .map(|l| size_key(l.size))
        .collect();
    heading_sizes.sort_unstable_by(|a, b| b.cmp(a));
    heading_sizes.dedup();
    let level_of = |l: &Line| -> u8 {
        let pos = heading_sizes
            .iter()
            .position(|k| *k == size_key(l.size))
            .unwrap_or(0);
        (pos + 1).min(6) as u8
    };

    // Typical baseline distance between consecutive body lines.
    let mut gaps: Vec<f64> = pages
        .iter()
        .flat_map(|p| p.lines.windows(2))
        .filter(|w| size_key(w[0].size) == size_key(body) && size_key(w[1].size) == size_key(body))
        .map(|w| w[1].y - w[0].y)
        .filter(|d| *d > 0.0 && *d < body * 3.0)
        .collect();
    let spacing = match median(&mut gaps) {
        s if s > 0.0 => s,
        _ => body * 1.2,
    };

    let classify = |l: &Line| -> LineClass {
        if l.size >= body * HEADING_RATIO {
            LineClass::Heading(level_of(l))
        } else if is_scene_break_text(&l.text) && !l.text.contains('-') {
            LineClass::SceneBreak
        } else if let Some(c) = BULLET
            .captures(&l.text)
            .filter(|_| !l.text.starts_with("--"))
        {
            LineClass::ListItem {
                ordered: false,
                index: 0,
                text: c[1].to_string(),
            }
        } else if let Some(c) = NUMBERED.captures(&l.text) {
            LineClass::ListItem {
                ordered: true,
                index: c[1].parse().unwrap_or(0),
                text: c[2].to_string(),
            }
        } else {
            LineClass::Body
        }
    };

    let mut asm = Assembler::default();
    let mut table: Option<TableRegion> = None;
    for page in &pages {
        let body_lines = page
            .lines
            .iter()
            .filter(|l| size_key(l.size) == size_key(body) || l.size < body);
        let left = body_lines.clone().map(|l| l.x0).fold(f64::MAX, f64::min);
        let right = body_lines.map(|l| l.x1).fold(f64::MIN, f64::max);
        let mut prev: Option<&Line> = None;
        let mut rules = page.rules.iter().copied().peekable();

        for line in &page.lines {
            // Inside a table, rules separate rows; the table ends at the next
            // heading, caption or line of running prose.
            if let Some(t) = table.as_mut() {
                while rules.next_if(|r| *r < line.y).is_some() {
                    t.end_row();
                }
                let ends = matches!(classify(line), LineClass::Heading(_))
                    || TABLE_CAPTION.is_match(&line.text)
                    || FIGURE_CAPTION.is_match(&line.text)
                    || is_prose(line, left, right)
                    || t.lines >= MAX_TABLE_LINES;
                if !ends {
                    t.add(line);
                    prev = Some(line);
                    continue;
                }
                let t = table.take().unwrap();
                asm.push(t.into_block(), page.num);
            }
            if matches!(classify(line), LineClass::Body) {
                if TABLE_CAPTION.is_match(&line.text) {
                    asm.flush();
                    table = Some(TableRegion::new(&line.text));
                    // Rules above the first row are part of the table.
                    while rules.next_if(|r| *r < line.y).is_some() {}
                    prev = Some(line);
                    continue;
                }
                if FIGURE_CAPTION.is_match(&line.text) {
                    asm.flush();
                    asm.push(Block::new(BlockKind::Figure, line.text.clone()), page.num);
                    prev = Some(line);
                    continue;
                }
            }
            // A rule lying between two text lines is a scene break.
            if rules.next_if(|r| *r < line.y).is_some() {
                while rules.next_if(|r| *r < line.y).is_some() {}
                if prev.is_some() {
                    asm.flush();
                    asm.push(Block::scene_break(), page.num);
                }
            }
            match classify(line) {
                LineClass::Heading(level) => {
                    asm.flush();
                    asm.heading(line, level, page.num);
                }
                LineClass::SceneBreak => {
                    asm.flush();
                    asm.push(Block::scene_break(), page.num);
                }
                LineClass::ListItem {
                    ordered,
                    index,
                    text,
                } => {
                    asm.flush();
                    asm.start(
                        BlockKind::ListItem { ordered, index },
                        &text,
                        line,
                        page.num,
                    );
                }
                LineClass::Body => {
                    let indent = line.x0 > left + 0.8 * line.size;
                    let new_para = match (&asm.open, prev) {
                        (None, _) => true,
                        (Some(open), Some(p)) => {
                            let gap = line.y - p.y > spacing * 1.6;
                            let in_list = matches!(open.kind, BlockKind::ListItem { .. });
                            let short_prev =
                                p.x1 < right - 4.0 * p.size && ends_sentence(&open.text);
                            if in_list {
                                gap || line.x0 < open.text_x0 - 0.5 * line.size
                            } else {
                                gap || indent || short_prev
                            }
                        }
                        // First line on a new page continues the open
                        // paragraph unless it is indented or the previous
                        // page ended on a short line.
                        (Some(open), None) => {
                            indent
                                || open.last_short
                                || matches!(open.kind, BlockKind::ListItem { .. })
                        }
                    };
                    if new_para {
                        asm.flush();
                        asm.start(BlockKind::Paragraph, &line.text, line, page.num);
                    } else {
                        asm.extend(line);
                    }
                }
            }
            if let Some(open) = asm.open.as_mut() {
                open.last_short = line.x1 < right - 4.0 * line.size;
            }
            prev = Some(line);
        }
    }
    asm.flush();
    if let Some(t) = table.take() {
        let page = asm.blocks.last().and_then(|b| b.page).unwrap_or(0);
        asm.push(t.into_block(), page);
    }
    for b in &mut asm.blocks {
        if b.is_heading() {
            b.text = normalise_heading(&b.text);
        }
    }
    asm.blocks
}

/// "C H A P T E R: 1: Dive In" -> "Chapter 1: Dive In".
fn normalise_heading(text: &str) -> String {
    let joined = LETTER_SPACED.replace_all(text, |c: &regex::Captures| c[0].replace(' ', ""));
    match NUMBERED_HEADING.captures(&joined) {
        Some(c) => {
            let label = c[1].to_lowercase();
            let label = label[..1].to_uppercase() + &label[1..];
            let rest = joined[c[0].len()..].trim();
            if rest.is_empty() {
                format!("{label} {}", &c[2])
            } else {
                format!("{label} {}: {rest}", &c[2])
            }
        }
        None => joined.into_owned(),
    }
}

/// A full-width line of ordinary running text (no column gaps): the end of
/// a table region.
fn is_prose(line: &Line, left: f64, right: f64) -> bool {
    line.cells.len() <= 1
        && line.x0 <= left + 1.0 * line.size
        && line.x1 >= right - 3.0 * line.size
        && line.text.chars().count() >= 50
}

/// Lines collected after a "Table N-N: caption" line. Rules split rows;
/// cells are placed in columns by their x position.
struct TableRegion {
    caption: String,
    rows: Vec<String>,
    /// Columns (x, text) of the row being collected.
    row: Vec<(f64, String)>,
    lines: usize,
}

impl TableRegion {
    fn new(caption: &str) -> Self {
        Self {
            caption: caption.trim().to_string(),
            rows: Vec::new(),
            row: Vec::new(),
            lines: 0,
        }
    }

    fn add(&mut self, line: &Line) {
        self.lines += 1;
        for (x, text) in &line.cells {
            let tol = 1.0 * line.size;
            match self.row.iter_mut().find(|(cx, _)| (cx - x).abs() <= tol) {
                Some((_, cell)) => {
                    let hyphenated = cell.ends_with('-')
                        && cell.chars().rev().nth(1).is_some_and(char::is_alphabetic)
                        && text.starts_with(|c: char| c.is_lowercase());
                    if hyphenated {
                        cell.pop();
                    } else {
                        cell.push(' ');
                    }
                    cell.push_str(text);
                }
                None => self.row.push((*x, text.clone())),
            }
        }
    }

    fn end_row(&mut self) {
        if self.row.is_empty() {
            return;
        }
        self.row.sort_by(|a, b| a.0.total_cmp(&b.0));
        let cells: Vec<&str> = self.row.iter().map(|(_, t)| t.as_str()).collect();
        self.rows.push(cells.join(" | "));
        self.row.clear();
    }

    fn into_block(mut self) -> Block {
        self.end_row();
        let mut text = self.caption;
        for r in self.rows {
            text.push('\n');
            text.push_str(&r);
        }
        Block::new(BlockKind::Table, text)
    }
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .ends_with(['.', '!', '?', ':', '"', '”', '’', '»'])
}

struct OpenBlock {
    kind: BlockKind,
    text: String,
    page: u32,
    text_x0: f64,
    last_short: bool,
}

#[derive(Default)]
struct Assembler {
    blocks: Vec<Block>,
    open: Option<OpenBlock>,
    /// Size key of the last heading line, for merging stacked headings.
    last_heading: Option<(i64, u32)>,
}

impl Assembler {
    fn start(&mut self, kind: BlockKind, text: &str, line: &Line, page: u32) {
        let text_x0 = if text.len() < line.text.len() {
            line.x0 + line.size
        } else {
            line.x0
        };
        self.open = Some(OpenBlock {
            kind,
            text: text.to_string(),
            page,
            text_x0,
            last_short: false,
        });
    }

    fn extend(&mut self, line: &Line) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let ends_hyphen = open.text.ends_with('-')
            && open
                .text
                .chars()
                .rev()
                .nth(1)
                .is_some_and(char::is_alphabetic);
        if ends_hyphen && line.text.chars().next().is_some_and(char::is_lowercase) {
            open.text.pop();
            open.text.push_str(&line.text);
        } else {
            open.text.push(' ');
            open.text.push_str(&line.text);
        }
    }

    fn flush(&mut self) {
        if let Some(open) = self.open.take() {
            let mut b = Block::new(open.kind, open.text);
            b.page = Some(open.page);
            self.blocks.push(b);
            self.last_heading = None;
        }
    }

    fn push(&mut self, mut block: Block, page: u32) {
        block.page = Some(page);
        self.blocks.push(block);
        self.last_heading = None;
    }

    /// Consecutive heading lines on a page form one heading: wrapped lines
    /// of the same size are joined with a space, a label above a title
    /// ("Chapter 1" / "The Lighthouse") with a colon.
    fn heading(&mut self, line: &Line, level: u8, page: u32) {
        let key = size_key(line.size);
        if let (Some((last_key, last_page)), Some(last)) =
            (self.last_heading, self.blocks.last_mut())
            && last_page == page
            && last.is_heading()
        {
            let sep = if last_key == key { " " } else { ": " };
            last.text = format!("{}{sep}{}", last.text, line.text);
            if let BlockKind::Heading { level: l } = &mut last.kind {
                *l = (*l).min(level);
            }
            self.last_heading = Some((key, page));
            return;
        }
        let mut b = Block::heading(level, line.text.clone());
        b.page = Some(page);
        self.blocks.push(b);
        self.last_heading = Some((key, page));
    }
}

// ---------------------------------------------------------------- metadata

fn read_info(doc: &pdf_extract::Document) -> BookMetadata {
    let mut meta = BookMetadata::default();
    let info = doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_dict().ok());
    if let Some(info) = info {
        let get = |key: &[u8]| -> Option<String> {
            let obj = info.get(key).ok()?;
            let (_, obj) = doc.dereference(obj).ok()?;
            pdf_extract::decode_text_string(obj)
                .ok()
                .and_then(non_empty)
        };
        meta.title = get(b"Title");
        meta.authors = get(b"Author")
            .map(|a| split_authors(&a))
            .unwrap_or_default();
        meta.description = get(b"Subject");
        meta.subjects = get(b"Keywords")
            .map(|k| k.split([',', ';']).filter_map(non_empty).collect())
            .unwrap_or_default();
    }
    meta.language = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Lang").ok())
        .and_then(|o| pdf_extract::decode_text_string(o).ok())
        .and_then(non_empty);
    meta
}

fn read_outline(doc: &pdf_extract::Document) -> Vec<TocEntry> {
    let toc = match catch_unwind(AssertUnwindSafe(|| doc.get_toc())) {
        Ok(Ok(toc)) => toc.toc,
        _ => return Vec::new(),
    };
    let min_level = toc.iter().map(|t| t.level).min().unwrap_or(1);
    toc.into_iter()
        .map(|t| TocEntry {
            // Outline strings are often NUL-padded UTF-16.
            title: super::collapse_ws(&t.title.replace(char::is_control, " ")),
            level: (t.level - min_level + 1).min(u8::MAX as usize) as u8,
            target: TocTarget::Page(t.page as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, x0: f64, y: f64, size: f64) -> Line {
        Line {
            text: text.into(),
            x0,
            x1: x0 + text.len() as f64 * size * 0.5,
            y,
            size,
            lead_mark: text.starts_with(|c: char| c.is_ascii_digit()),
            cells: vec![(x0, text.into())],
        }
    }

    /// A table line: cells at the given x positions.
    fn row(cells: &[(&str, f64)], y: f64) -> Line {
        let text = cells.iter().map(|c| c.0).collect::<Vec<_>>().join(" ");
        let last = cells.last().unwrap();
        Line {
            text,
            x0: cells[0].1,
            x1: last.1 + last.0.len() as f64 * 5.0,
            y,
            size: 10.0,
            lead_mark: false,
            cells: cells.iter().map(|(t, x)| (*x, t.to_string())).collect(),
        }
    }

    fn glyphs(spec: &[(&str, f64, f64)]) -> Vec<Glyph> {
        spec.iter()
            .map(|(t, x, end)| Glyph {
                x: *x * 10.0,
                y: 100.0,
                end: *end * 10.0,
                size: 10.0,
                text: t.to_string(),
            })
            .collect()
    }

    #[test]
    fn ignores_spaces_overlapping_ligatures() {
        // Positions in ems, as measured in a Wiley PDF.
        let line = render_line(glyphs(&[
            ("o", 17.0, 17.5),
            ("fi", 17.5, 18.1),
            (" ", 17.8, 18.05),
            ("n", 18.1, 18.6),
            ("g", 18.6, 19.1),
            (" ", 19.1, 19.35),
            ("a", 19.35, 19.85),
            ("f", 20.0, 20.33),
            (" ", 20.33, 20.58),
            ("fi", 20.54, 21.14),
            ("x", 21.14, 21.6),
        ]))
        .unwrap();
        assert_eq!(line.text, "ofing affix");
    }

    #[test]
    fn splits_wide_gaps_into_cells() {
        let line = render_line(glyphs(&[
            ("a", 1.0, 1.5),
            (" ", 1.5, 1.75),
            ("b", 1.75, 2.25),
            ("c", 6.0, 6.5),
        ]))
        .unwrap();
        assert_eq!(line.text, "a b c");
        assert_eq!(
            line.cells,
            vec![(10.0, "a b".to_string()), (60.0, "c".to_string())]
        );
    }

    #[test]
    fn normalises_letter_spaced_chapter_headings() {
        assert_eq!(
            normalise_heading("C H A P T E R: 1: Dive In and Model!"),
            "Chapter 1: Dive In and Model!"
        );
        assert_eq!(
            normalise_heading("PART II Finding Things"),
            "Part II: Finding Things"
        );
        assert_eq!(normalise_heading("Part Two"), "Part Two");
        assert_eq!(normalise_heading("Appendix A Tools"), "Appendix A: Tools");
        assert_eq!(
            normalise_heading("A Plan for Testing"),
            "A Plan for Testing"
        );
    }

    #[test]
    fn collects_tables_and_figures() {
        let full = "x".repeat(80);
        let pages = vec![Page {
            num: 1,
            height: 800.0,
            rules: vec![130.0, 160.0, 200.0],
            lines: vec![
                line(&format!("Intro {full}"), 100.0, 100.0, 10.0),
                line("Table 2-1: Some Things", 100.0, 112.0, 10.0),
                row(&[("THING", 100.0), ("NOTES", 300.0)], 124.0),
                row(
                    &[("Tampering with a", 100.0), ("Hard to defend", 300.0)],
                    136.0,
                ),
                row(&[("file", 100.0), ("once authen-", 300.0)], 148.0),
                row(&[("tication fails", 300.0)], 152.0),
                row(&[("Spoofing", 100.0), ("Easy", 300.0)], 172.0),
                line(&format!("After the table {full}"), 100.0, 212.0, 10.0),
                line("Figure 2-1: A diagram", 100.0, 224.0, 10.0),
                line(&format!("More text {full}"), 100.0, 236.0, 10.0),
            ],
        }];
        let blocks = build_blocks(pages);
        let got: Vec<(BlockKind, &str)> =
            blocks.iter().map(|b| (b.kind, b.text.as_str())).collect();
        assert_eq!(got.len(), 5, "{got:?}");
        assert_eq!(
            got[1],
            (
                BlockKind::Table,
                "Table 2-1: Some Things
THING | NOTES
Tampering with a file | Hard to defend once authentication fails
Spoofing | Easy"
            )
        );
        assert_eq!(got[2].0, BlockKind::Paragraph);
        assert_eq!(got[3], (BlockKind::Figure, "Figure 2-1: A diagram"));
    }

    #[test]
    fn rebuilds_paragraphs_headings_and_strips_furniture() {
        let full = "x".repeat(80); // full-width line
        let pages = vec![
            Page {
                num: 1,
                height: 800.0,
                rules: vec![],
                lines: vec![
                    line("Chapter 1", 100.0, 100.0, 20.0),
                    line("The Lighthouse", 100.0, 130.0, 24.0),
                    line(&format!("{full} exam-"), 100.0, 200.0, 10.0),
                    line(&format!("ple {full}"), 100.0, 212.0, 10.0),
                    line("ends here.", 100.0, 224.0, 10.0),
                    line(&format!("Indented {full}"), 115.0, 236.0, 10.0),
                    line(&format!("{full} {full}"), 100.0, 248.0, 10.0),
                    line("1 A footnote in small type.", 100.0, 700.0, 8.0),
                    line("12", 300.0, 760.0, 10.0),
                ],
            },
            Page {
                num: 2,
                height: 800.0,
                rules: vec![106.0],
                lines: vec![
                    line("continued on the next page.", 100.0, 100.0, 10.0),
                    line("• An item", 100.0, 112.0, 10.0),
                    line("13", 300.0, 760.0, 10.0),
                ],
            },
        ];
        let blocks = build_blocks(pages);
        let got: Vec<(BlockKind, String, Option<u32>)> = blocks
            .iter()
            .map(|b| (b.kind, b.text.clone(), b.page))
            .collect();
        assert_eq!(
            got[0],
            (
                BlockKind::Heading { level: 1 },
                "Chapter 1: The Lighthouse".into(),
                Some(1)
            )
        );
        assert_eq!(got[1].1, format!("{full} example {full} ends here."));
        assert_eq!(
            got[2].1,
            format!("Indented {full} {full} {full} continued on the next page.")
        );
        assert_eq!(got[3], (BlockKind::SceneBreak, String::new(), Some(2)));
        assert_eq!(
            got[4],
            (
                BlockKind::ListItem {
                    ordered: false,
                    index: 0
                },
                "An item".into(),
                Some(2)
            )
        );
        assert_eq!(got.len(), 5);
    }
}
