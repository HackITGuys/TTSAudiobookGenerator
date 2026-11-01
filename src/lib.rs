//! TTS audiobook generator.
//!
//! Stage 1 (this crate today): extract structured text from PDF, EPUB, DOCX
//! or Markdown, split it into chapters, clean it for narration and emit
//! TTS-ready segments plus SSML.

pub mod assemble;
pub mod chapters;
pub mod clean;
pub mod credits;
pub mod extract;
pub mod mistral;
pub mod model;
pub mod output;
pub mod sanitize;
pub mod segment;
pub mod sentences;
pub mod ssml;
pub mod synthesize;

use std::path::Path;

use anyhow::Result;

use crate::chapters::{SplitOptions, split_chapters};
use crate::model::Book;
use crate::segment::{DEFAULT_MAX_SEGMENT_CHARS, segment_chapter};

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    /// Characters per TTS request (Voxtral accepts 4000); segments end at a full stop.
    pub max_segment_chars: usize,
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
        chapter.segments = segment_chapter(chapter, opts.max_segment_chars);
    }

    Ok(Book {
        generator: format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        source_file: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        metadata,
        chapters,
        tts: None,
        credits: None,
        cover: raw.cover,
    })
}
