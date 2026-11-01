//! Cut a chapter into plain-text segments small enough for one TTS request.
//!
//! Mistral's TTS takes plain text only (no SSML) and works best with fewer
//! than ~300 words per request, so pauses are carried as `pause_after_ms` and
//! inserted as silence by the audio stage.

use crate::model::{BlockKind, Chapter, Segment, word_count};
use crate::sentences::split_sentences;

pub const TITLE_PAUSE_MS: u32 = 1500;
pub const HEADING_PAUSE_MS: u32 = 800;
pub const SCENE_BREAK_PAUSE_MS: u32 = 2000;
pub const PARAGRAPH_PAUSE_MS: u32 = 600;
pub const LIST_ITEM_PAUSE_MS: u32 = 300;
pub const MID_PARAGRAPH_PAUSE_MS: u32 = 250;

pub const DEFAULT_MAX_SEGMENT_WORDS: usize = 300;

/// Text of a list item as it should be spoken.
pub fn list_item_text(kind: BlockKind, text: &str) -> String {
    match kind {
        BlockKind::ListItem {
            ordered: true,
            index,
        } => format!("{index}. {text}"),
        _ => text.to_string(),
    }
}

struct Packer {
    max_words: usize,
    segments: Vec<Segment>,
    parts: Vec<String>,
    words: usize,
    pause: u32,
}

impl Packer {
    fn flush(&mut self) {
        if self.parts.is_empty() {
            return;
        }
        self.segments.push(Segment {
            text: self.parts.join("\n"),
            words: self.words,
            pause_after_ms: self.pause,
        });
        self.parts.clear();
        self.words = 0;
    }

    fn push_unit(&mut self, text: String, pause: u32) {
        let words = word_count(&text);
        if words > self.max_words {
            self.flush();
            let pieces = split_long(&text, self.max_words);
            let last = pieces.len() - 1;
            for (i, piece) in pieces.into_iter().enumerate() {
                self.parts.push(piece);
                self.words = word_count(&self.parts[0]);
                self.pause = if i == last {
                    pause
                } else {
                    MID_PARAGRAPH_PAUSE_MS
                };
                if i != last {
                    self.flush();
                }
            }
        } else {
            if self.words + words > self.max_words {
                self.flush();
            }
            self.parts.push(text);
            self.words += words;
            self.pause = pause;
        }
        if pause >= HEADING_PAUSE_MS {
            self.flush();
        }
    }

    fn scene_break(&mut self) {
        if !self.parts.is_empty() {
            self.pause = self.pause.max(SCENE_BREAK_PAUSE_MS);
            self.flush();
        } else if let Some(last) = self.segments.last_mut() {
            last.pause_after_ms = last.pause_after_ms.max(SCENE_BREAK_PAUSE_MS);
        }
    }
}

/// Split a paragraph that exceeds `max_words` at sentence boundaries,
/// falling back to plain word boundaries for monster sentences.
fn split_long(text: &str, max_words: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut cur_words = 0;
    for sentence in split_sentences(text) {
        let words = word_count(sentence);
        if words > max_words {
            if !cur.is_empty() {
                pieces.push(cur.join(" "));
                cur.clear();
                cur_words = 0;
            }
            let all: Vec<&str> = sentence.split_whitespace().collect();
            pieces.extend(all.chunks(max_words).map(|c| c.join(" ")));
            continue;
        }
        if cur_words + words > max_words {
            pieces.push(cur.join(" "));
            cur.clear();
            cur_words = 0;
        }
        cur.push(sentence);
        cur_words += words;
    }
    if !cur.is_empty() {
        pieces.push(cur.join(" "));
    }
    pieces
}

/// Build the TTS segments for a chapter. The chapter title is always a
/// segment of its own so chapter markers line up with the spoken title.
pub fn segment_chapter(chapter: &Chapter, max_words: usize) -> Vec<Segment> {
    let mut p = Packer {
        max_words: max_words.max(1),
        segments: Vec::new(),
        parts: Vec::new(),
        words: 0,
        pause: PARAGRAPH_PAUSE_MS,
    };
    p.push_unit(
        format!("{}.", chapter.title.trim_end_matches(['.', ':'])),
        TITLE_PAUSE_MS,
    );

    for block in &chapter.blocks {
        match block.kind {
            BlockKind::Heading { .. } => {
                p.flush();
                p.push_unit(block.text.clone(), HEADING_PAUSE_MS);
            }
            BlockKind::Paragraph => p.push_unit(block.text.clone(), PARAGRAPH_PAUSE_MS),
            BlockKind::ListItem { .. } => {
                p.push_unit(list_item_text(block.kind, &block.text), LIST_ITEM_PAUSE_MS)
            }
            BlockKind::SceneBreak => p.scene_break(),
        }
    }
    p.flush();
    p.segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Block;

    fn chapter(blocks: Vec<Block>) -> Chapter {
        Chapter::new(1, "Chapter 1: The Lighthouse".into(), blocks)
    }

    #[test]
    fn title_is_its_own_segment() {
        let segs = segment_chapter(&chapter(vec![Block::paragraph("One. Two.")]), 300);
        assert_eq!(segs[0].text, "Chapter 1: The Lighthouse.");
        assert_eq!(segs[0].pause_after_ms, TITLE_PAUSE_MS);
        assert_eq!(segs[1].text, "One. Two.");
        assert_eq!(segs[1].pause_after_ms, PARAGRAPH_PAUSE_MS);
    }

    #[test]
    fn packs_paragraphs_up_to_limit() {
        let para = "word ".repeat(40);
        let blocks = (0..5).map(|_| Block::paragraph(para.trim())).collect();
        let segs = segment_chapter(&chapter(blocks), 100);
        // title + 3 segments of (2, 2, 1) paragraphs
        assert_eq!(segs.len(), 4);
        assert!(segs.iter().all(|s| s.words <= 100));
        assert_eq!(segs[1].words, 80);
    }

    #[test]
    fn splits_long_paragraph_at_sentences() {
        let sentence = format!("Word {}end.", "word ".repeat(28)); // 30 words
        let para = vec![sentence; 5].join(" ");
        let segs = segment_chapter(&chapter(vec![Block::paragraph(para)]), 70);
        let body = &segs[1..];
        assert_eq!(
            body.iter().map(|s| s.words).collect::<Vec<_>>(),
            vec![60, 60, 30]
        );
        assert_eq!(body[0].pause_after_ms, MID_PARAGRAPH_PAUSE_MS);
        assert_eq!(body[2].pause_after_ms, PARAGRAPH_PAUSE_MS);
    }

    #[test]
    fn scene_breaks_and_headings_end_segments() {
        let blocks = vec![
            Block::paragraph("Before."),
            Block::scene_break(),
            Block::paragraph("After."),
            Block::heading(3, "A Section"),
            Block::paragraph("Inside."),
        ];
        let segs = segment_chapter(&chapter(blocks), 300);
        let summary: Vec<(&str, u32)> = segs
            .iter()
            .map(|s| (s.text.as_str(), s.pause_after_ms))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Chapter 1: The Lighthouse.", TITLE_PAUSE_MS),
                ("Before.", SCENE_BREAK_PAUSE_MS),
                ("After.", PARAGRAPH_PAUSE_MS),
                ("A Section", HEADING_PAUSE_MS),
                ("Inside.", PARAGRAPH_PAUSE_MS),
            ]
        );
    }
}
