//! TTS audiobook generator.
//!
//! Stages:
//! 1. `extract`: structured text from PDF, EPUB, DOCX or Markdown, split into
//!    chapters and cleaned for narration; written as `raw/*.md`.
//! 2. `refine` (optional): an LLM repairs extraction damage and rewrites
//!    tables, figures, code and identifiers for speech; written as `text/*.md`.
//! 3. `build`: TTS-ready segments (`book.json`) plus SSML from `text/`, or
//!    from `raw/` for chapters that were not refined.
//! 4. `synthesize` and `assemble`: audio.

pub mod abridge;
pub mod assemble;
pub mod chapters;
pub mod clean;
pub mod credits;
pub mod extract;
pub mod llm;
pub mod mistral;
pub mod model;
pub mod output;
pub mod refine;
pub mod sanitize;
pub mod segment;
pub mod sentences;
pub mod ssml;
pub mod synthesize;

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::chapters::{SplitOptions, split_chapters};
use crate::model::{Book, Chapter};
use crate::segment::{DEFAULT_MAX_SEGMENT_CHARS, DEFAULT_MIN_SEGMENT_CHARS, segment_chapter_with};

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    /// Characters per TTS request (Voxtral accepts 4000); segments end at a full stop.
    pub max_segment_chars: usize,
    /// Segments shorter than this are merged into a neighbour when they fit.
    pub min_segment_chars: usize,
    /// Keep chapters such as "Contents" or "Notes" that are normally skipped.
    pub keep_all: bool,
    /// Ignore the source's table of contents and split on headings only.
    pub ignore_toc: bool,
    /// Replace hashes, long numbers, base64 and over-long words with a
    /// spoken placeholder before splitting into chapters and segments.
    pub sanitize: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            max_segment_chars: DEFAULT_MAX_SEGMENT_CHARS,
            min_segment_chars: DEFAULT_MIN_SEGMENT_CHARS,
            keep_all: false,
            ignore_toc: false,
            sanitize: true,
        }
    }
}

/// Run the full text pipeline: extract -> clean -> split -> segment.
pub fn extract_book(path: &Path, opts: &ExtractOptions) -> Result<Book> {
    let raw = extract::extract(path)?;
    let mut metadata = raw.metadata;
    if metadata.title.is_none() {
        metadata.title = path
            .file_stem()
            .map(|s| s.to_string_lossy().replace(['_', '-'], " "));
    }

    // Cleaning (including sanitising) runs before chapter splitting and
    // segmenting, so dots inside codes cannot create false sentence breaks and
    // segment lengths are measured on the text that will be spoken.
    let blocks = clean::clean_blocks(
        raw.blocks,
        clean::CleanOptions {
            sanitize: opts.sanitize,
        },
    );
    let toc = if opts.ignore_toc {
        &[][..]
    } else {
        &raw.toc[..]
    };
    let mut chapters = split_chapters(
        blocks,
        toc,
        &SplitOptions {
            book_title: metadata.title.as_deref(),
            keep_all: opts.keep_all,
        },
    );
    for chapter in &mut chapters {
        chapter.segments =
            segment_chapter_with(chapter, opts.max_segment_chars, opts.min_segment_chars);
    }

    Ok(Book {
        generator: format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        source_file: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        metadata,
        text_stage: Some("raw".into()),
        chapters,
        tts: None,
        credits: None,
        cover: raw.cover,
    })
}

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub max_segment_chars: usize,
    pub min_segment_chars: usize,
    /// Replace hashes, long numbers and similar tokens (see `sanitize`).
    pub sanitize: bool,
    pub ssml: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            max_segment_chars: DEFAULT_MAX_SEGMENT_CHARS,
            min_segment_chars: DEFAULT_MIN_SEGMENT_CHARS,
            sanitize: true,
            ssml: true,
        }
    }
}

/// The Markdown a chapter is built from: its refined text if that file
/// exists, else its raw extraction. Returns the path relative to `dir`.
pub fn chapter_source(dir: &Path, chapter: &Chapter) -> Option<(String, bool)> {
    if let Some(rel) = &chapter.refined_file
        && dir.join(rel).is_file()
    {
        return Some((rel.clone(), true));
    }
    chapter
        .raw_file
        .as_ref()
        .filter(|rel| dir.join(rel).is_file())
        .map(|rel| (rel.clone(), false))
}

/// Rebuild the segments, `chapters/*.txt|.ssml` and `book.json` of an
/// extracted book from its Markdown (`text/` where refined, else `raw/`).
/// Hand edits to those files are picked up here.
pub fn build_book(dir: &Path, opts: &BuildOptions) -> Result<Book> {
    let (mut book, dir) = synthesize::load_book(dir)?;
    let clean_opts = clean::CleanOptions {
        sanitize: opts.sanitize,
    };
    let mut refined = 0;
    let mut chapters = Vec::with_capacity(book.chapters.len());
    for old in std::mem::take(&mut book.chapters) {
        let Some((rel, is_refined)) = chapter_source(&dir, &old) else {
            bail!(
                "chapter {} ({}) has no raw/ or text/ Markdown; re-run `extract` with this version",
                old.index,
                old.title
            );
        };
        let md = std::fs::read_to_string(dir.join(&rel))
            .with_context(|| format!("cannot read {}", dir.join(&rel).display()))?;
        let mut blocks = extract::text::parse(&md).blocks;
        // The first `# ` line is the chapter title (hand edits win).
        let title = match blocks.first() {
            Some(b) if b.heading_level() == Some(1) => blocks.remove(0).text,
            _ => old.title.clone(),
        };
        let blocks = clean::clean_blocks(blocks, clean_opts);
        let mut chapter = Chapter::new(old.index, title, blocks);
        chapter.raw_file = old.raw_file;
        chapter.refined_file = old.refined_file;
        chapter.segments =
            segment_chapter_with(&chapter, opts.max_segment_chars, opts.min_segment_chars);
        refined += usize::from(is_refined);
        chapters.push(chapter);
    }
    book.chapters = chapters;
    book.text_stage = Some(
        match refined {
            0 => "raw",
            n if n == book.chapters.len() => "refined",
            _ => "partial",
        }
        .into(),
    );
    output::write_chapter_files(&mut book, &dir, opts.ssml)?;
    Ok(book)
}
