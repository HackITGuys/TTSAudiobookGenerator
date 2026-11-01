//! Core data model shared by every pipeline stage.
//!
//! `Book` is what gets serialised to `book.json`; it is the contract between
//! text extraction and the later TTS / audio assembly stages.

use serde::{Deserialize, Serialize};

/// Bibliographic metadata gathered from the source file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BookMetadata {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub authors: Vec<String>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    pub published: Option<String>,
    pub identifier: Option<String>,
    pub description: Option<String>,
    pub subjects: Vec<String>,
    pub source_format: String,
    /// File name of the extracted cover image, relative to the book output dir.
    pub cover_file: Option<String>,
}

/// The structural role of a block of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Heading { level: u8 },
    Paragraph,
    ListItem { ordered: bool, index: u32 },
    SceneBreak,
}

/// A unit of document text with enough positional information to map
/// table-of-contents entries back onto it.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
    /// 1-based page number (PDF only).
    pub page: Option<u32>,
    /// Index of the source document within the book (EPUB spine position).
    pub doc: Option<usize>,
    /// Element ids that point at (or just before) this block (EPUB anchors).
    pub anchors: Vec<String>,
}

impl Block {
    pub fn new(kind: BlockKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            page: None,
            doc: None,
            anchors: Vec::new(),
        }
    }

    pub fn heading(level: u8, text: impl Into<String>) -> Self {
        Self::new(BlockKind::Heading { level }, text)
    }

    pub fn paragraph(text: impl Into<String>) -> Self {
        Self::new(BlockKind::Paragraph, text)
    }

    pub fn scene_break() -> Self {
        Self::new(BlockKind::SceneBreak, "")
    }

    pub fn is_heading(&self) -> bool {
        matches!(self.kind, BlockKind::Heading { .. })
    }

    pub fn heading_level(&self) -> Option<u8> {
        match self.kind {
            BlockKind::Heading { level } => Some(level),
            _ => None,
        }
    }

    pub fn word_count(&self) -> usize {
        word_count(&self.text)
    }
}

/// Where a table-of-contents entry points inside the source.
#[derive(Debug, Clone, PartialEq)]
pub enum TocTarget {
    /// 1-based PDF page.
    Page(u32),
    /// EPUB spine document, optionally narrowed to an element id.
    Doc { doc: usize, anchor: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TocEntry {
    pub title: String,
    /// 1 = top level.
    pub level: u8,
    pub target: TocTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cover {
    pub bytes: Vec<u8>,
    /// File extension without the dot, e.g. `jpg`.
    pub extension: String,
}

/// A piece of plain text sized for one TTS request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub words: usize,
    /// Unicode characters in `text` (the TTS request limit is in characters).
    #[serde(default)]
    pub chars: usize,
    /// Silence the audio stage should insert after this segment.
    pub pause_after_ms: u32,
    /// Synthesised audio, relative to the book output dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_file: Option<String>,
    /// Used instead of `audio_file` when moderation forced the segment to be
    /// synthesised in pieces; played back to back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_parts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    /// 1-based position in the book.
    pub index: usize,
    pub title: String,
    pub word_count: usize,
    pub est_duration_secs: u32,
    /// Plain-text file, relative to the book output dir.
    pub text_file: Option<String>,
    /// SSML file, relative to the book output dir.
    pub ssml_file: Option<String>,
    pub segments: Vec<Segment>,
    #[serde(skip)]
    pub blocks: Vec<Block>,
}

/// Speaking rate used for duration estimates (words per minute).
pub const NARRATION_WPM: f64 = 155.0;

impl Chapter {
    pub fn new(index: usize, title: String, blocks: Vec<Block>) -> Self {
        let word_count = word_count(&title) + blocks.iter().map(Block::word_count).sum::<usize>();
        Self {
            index,
            title,
            word_count,
            est_duration_secs: (word_count as f64 / NARRATION_WPM * 60.0).round() as u32,
            text_file: None,
            ssml_file: None,
            segments: Vec::new(),
            blocks,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Book {
    pub generator: String,
    pub source_file: String,
    pub metadata: BookMetadata,
    pub chapters: Vec<Chapter>,
    /// Voice settings used for the audio, once synthesised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts: Option<TtsInfo>,
    /// Spoken opening credits and closing AI disclaimer (made by `synthesize`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    #[serde(skip)]
    pub cover: Option<Cover>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credits {
    pub opening: Vec<Segment>,
    pub closing: Vec<Segment>,
}

/// Which TTS model and voice produced the audio (needed for the AI disclaimer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TtsInfo {
    pub provider: String,
    pub model: String,
    pub voice: Option<String>,
    pub format: String,
}

/// Output of a format-specific extractor, before cleaning and chapter splitting.
#[derive(Debug, Clone, Default)]
pub struct RawDocument {
    pub metadata: BookMetadata,
    pub blocks: Vec<Block>,
    pub toc: Vec<TocEntry>,
    pub cover: Option<Cover>,
}

pub fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}
