//! Cut a chapter into plain-text segments small enough for one TTS request.
//!
//! Mistral's Voxtral TTS takes plain text only (no SSML) and accepts at most
//! 4000 characters per request. Segments are packed up to that limit and
//! always end at a sentence boundary; only a single sentence longer than the
//! limit is cut elsewhere (at a comma or similar, else between words).
//! Pauses are carried as `pause_after_ms` and inserted as silence by the
//! audio stage.

use crate::model::{BlockKind, Chapter, Segment, word_count};
use crate::sentences::split_sentences;

pub const TITLE_PAUSE_MS: u32 = 1500;
pub const HEADING_PAUSE_MS: u32 = 800;
pub const SCENE_BREAK_PAUSE_MS: u32 = 2000;
pub const PARAGRAPH_PAUSE_MS: u32 = 600;
pub const LIST_ITEM_PAUSE_MS: u32 = 300;
pub const MID_PARAGRAPH_PAUSE_MS: u32 = 250;

/// Voxtral TTS request limit.
pub const DEFAULT_MAX_SEGMENT_CHARS: usize = 4000;

/// Separator between paragraphs packed into one segment.
const PARA_SEP: &str = "\n";

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

fn char_len(s: &str) -> usize {
    s.chars().count()
}

pub fn segment(text: String, pause_after_ms: u32) -> Segment {
    Segment {
        words: word_count(&text),
        chars: char_len(&text),
        text,
        pause_after_ms,
        audio_file: None,
        audio_parts: Vec::new(),
    }
}

struct Packer {
    max_chars: usize,
    segments: Vec<Segment>,
    parts: Vec<String>,
    /// Characters of `parts` joined with `PARA_SEP`.
    chars: usize,
    pause: u32,
}

impl Packer {
    fn flush(&mut self) {
        if self.parts.is_empty() {
            return;
        }
        self.segments
            .push(segment(self.parts.join(PARA_SEP), self.pause));
        self.parts.clear();
        self.chars = 0;
    }

    /// Length of the current segment if `len` more characters were added.
    fn with(&self, len: usize) -> usize {
        if self.parts.is_empty() {
            len
        } else {
            self.chars + PARA_SEP.len() + len
        }
    }

    /// Append a piece that fits the limit on its own.
    fn add(&mut self, text: String, pause: u32) {
        let len = char_len(&text);
        if self.with(len) > self.max_chars {
            self.flush();
        }
        self.chars = self.with(len);
        self.parts.push(text);
        self.pause = pause;
    }

    fn push_unit(&mut self, text: String, pause: u32) {
        if char_len(&text) > self.max_chars {
            let pieces = split_long(&text, self.max_chars);
            let last = pieces.len() - 1;
            for (i, piece) in pieces.into_iter().enumerate() {
                if i == last {
                    self.add(piece, pause);
                } else {
                    self.add(piece, MID_PARAGRAPH_PAUSE_MS);
                    self.flush();
                }
            }
        } else {
            self.add(text, pause);
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

/// Split a paragraph longer than `max` characters into pieces that end at
/// full stops. A sentence that alone exceeds `max` is cut by `split_sentence`.
fn split_long(text: &str, max: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0;
    for sentence in split_sentences(text) {
        let len = char_len(sentence);
        if len > max {
            if !cur.is_empty() {
                pieces.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            pieces.extend(split_sentence(sentence, max));
            continue;
        }
        let needed = if cur.is_empty() {
            len
        } else {
            cur_len + 1 + len
        };
        if needed > max {
            pieces.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if !cur.is_empty() {
            cur.push(' ');
            cur_len += 1;
        }
        cur.push_str(sentence);
        cur_len += len;
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

/// Cut one over-long sentence: at the last clause break (`,;:—–)`) that
/// fits, else at the last space, else hard at `max` characters.
fn split_sentence(sentence: &str, max: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = sentence.trim();
    while char_len(rest) > max {
        // Byte offset of the first character beyond the limit.
        let limit = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        let window = &rest[..limit];
        let clause = window
            .char_indices()
            .filter(|&(i, c)| {
                matches!(c, ',' | ';' | ':' | '—' | '–' | ')')
                    && rest[i + c.len_utf8()..].starts_with(' ')
            })
            .map(|(i, c)| i + c.len_utf8())
            .next_back()
            .filter(|&i| i > limit / 2);
        let cut = clause
            .or_else(|| window.rfind(' ').filter(|&i| i > 0))
            .unwrap_or(limit);
        pieces.push(rest[..cut].trim().to_string());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        pieces.push(rest.to_string());
    }
    pieces
}

/// Build the TTS segments for a chapter. The chapter title is always a
/// segment of its own so chapter markers line up with the spoken title.
pub fn segment_chapter(chapter: &Chapter, max_chars: usize) -> Vec<Segment> {
    let mut p = Packer {
        max_chars: max_chars.max(1),
        segments: Vec::new(),
        parts: Vec::new(),
        chars: 0,
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
        let segs = segment_chapter(&chapter(vec![Block::paragraph("One. Two.")]), 4000);
        assert_eq!(segs[0].text, "Chapter 1: The Lighthouse.");
        assert_eq!(segs[0].pause_after_ms, TITLE_PAUSE_MS);
        assert_eq!(segs[1].text, "One. Two.");
        assert_eq!(segs[1].chars, 9);
        assert_eq!(segs[1].pause_after_ms, PARAGRAPH_PAUSE_MS);
    }

    #[test]
    fn packs_paragraphs_up_to_char_limit() {
        let para = "x".repeat(40);
        let blocks = (0..5).map(|_| Block::paragraph(para.clone())).collect();
        let segs = segment_chapter(&chapter(blocks), 100);
        // 40 + 1 + 40 = 81 fits; a third paragraph would make 122.
        let body: Vec<usize> = segs[1..].iter().map(|s| s.chars).collect();
        assert_eq!(body, vec![81, 81, 40]);
    }

    #[test]
    fn splits_long_paragraph_at_full_stops() {
        let sentence = format!("Word {}end.", "word ".repeat(28));
        let len = sentence.chars().count();
        let para = vec![sentence; 5].join(" ");
        let segs = segment_chapter(&chapter(vec![Block::paragraph(para)]), 2 * len + 10);
        let body = &segs[1..];
        assert_eq!(body.len(), 3);
        assert!(body.iter().all(|s| s.text.ends_with("end.")));
        assert_eq!(body[0].chars, 2 * len + 1);
        assert_eq!(body[0].pause_after_ms, MID_PARAGRAPH_PAUSE_MS);
        assert_eq!(body[2].pause_after_ms, PARAGRAPH_PAUSE_MS);
    }

    #[test]
    fn cuts_monster_sentence_at_clause_breaks() {
        let sentence = format!(
            "{}, and finally the end.",
            ["very long clause"; 20].join(", ")
        );
        let pieces = split_sentence(&sentence, 60);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| p.chars().count() <= 60));
        assert!(pieces[..pieces.len() - 1].iter().all(|p| p.ends_with(',')));
        assert_eq!(pieces.join(" "), sentence);
        // No spaces at all: hard cut.
        assert_eq!(split_sentence("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // No clause marks: cut between words.
        assert_eq!(
            split_sentence("aaa bbb ccc ddd", 8),
            vec!["aaa bbb", "ccc ddd"]
        );
    }

    #[test]
    fn never_exceeds_limit() {
        let text = "Short. ".repeat(50) + &"Ünïcödé wörds ".repeat(30) + "done.";
        let segs = segment_chapter(&chapter(vec![Block::paragraph(text)]), 50);
        assert!(
            segs.iter()
                .all(|s| s.chars <= 50 && s.chars == s.text.chars().count())
        );
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
        let segs = segment_chapter(&chapter(blocks), 4000);
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
