//! Text normalisation for TTS.
//!
//! Mistral's TTS takes plain text and recommends avoiding markdown, emoji and
//! special characters, so everything that would be read out as noise
//! (footnote markers, citations, URLs, layout glyphs) is removed here.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::model::{Block, BlockKind};

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}

re!(
    INVISIBLE,
    "[\u{00AD}\u{200B}\u{200C}\u{200D}\u{2060}\u{FEFF}]"
);
re!(
    ODD_SPACES,
    "[\u{00A0}\u{2000}-\u{200A}\u{202F}\u{205F}\u{3000}\t\r\n]"
);
re!(SQUARED_UNITS, r"\b(m|cm|mm|km|ft)²");
re!(CUBED_UNITS, r"\b(m|cm|mm|km|ft)³");
re!(SUPERSCRIPT_MARKERS, r"([\p{L}\p{P}])[⁰¹²³⁴⁵⁶⁷⁸⁹]+");
re!(
    BRACKET_REFS,
    r"(?i)\s?\[(?:\d+(?:\s*[-–,]\s*\d+)*|citation needed|note \d+|fn ?\d+|\^[^\]]+)\]"
);
re!(TRAILING_MARKS, r"([\p{L}.,;:!?])[*†‡]+(\s|$)");
re!(URL, r"(?i)\b(?:https?://|www\.)\S+");
re!(DOT_LEADER, r"(?:\.\s?){4,}\s*\d+\s*$");
re!(MARKUP_CHARS, r"[*_`#|~^]");
re!(LEADING_BULLET, r"^[•●▪■►▶◦‣⁃∙·–-]\s+");
re!(
    EMOJI,
    "[\u{1F000}-\u{1FAFF}\u{2600}-\u{27BF}\u{2B00}-\u{2BFF}\u{FE0F}\u{20E3}\u{E0020}-\u{E007F}]"
);
re!(SPACE_BEFORE_PUNCT, r"\s+([.,;:!?)\]])");
re!(EMPTY_PARENS, r"\(\s*\)|\[\s*\]");
re!(MULTI_SPACE, r" {2,}");

/// One author-year reference: `Smith 2019`, `Smith et al., 2019a, p. 4`,
/// `Smith & Jones 2019`.
const AUTHOR_YEAR: &str = r"[A-Z][\p{L}'’\-]+(?:\s+(?:et\s+al\.?|and|&)\s*(?:[A-Z][\p{L}'’\-]+)?)*,?\s+\d{4}[a-z]?(?:,\s*(?:p|pp)\.?\s*\d+(?:\s*[-–]\s*\d+)?)?";

static CITATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"\s?\((?:see\s+|cf\.\s+)?{AUTHOR_YEAR}(?:;\s*{AUTHOR_YEAR})*\)"
    ))
    .unwrap()
});

const LIGATURES: &[(char, &str)] = &[
    ('ﬀ', "ff"),
    ('ﬁ', "fi"),
    ('ﬂ', "fl"),
    ('ﬃ', "ffi"),
    ('ﬄ', "ffl"),
    ('ﬅ', "ft"),
    ('ﬆ', "st"),
];

/// Clean a single piece of text for narration. Returns an empty string if
/// nothing speakable is left.
pub fn clean_text(input: &str) -> String {
    let mut s = String::with_capacity(input.len());
    for c in input.chars() {
        match LIGATURES.iter().find(|(l, _)| *l == c) {
            Some((_, rep)) => s.push_str(rep),
            None => s.push(c),
        }
    }

    let s = INVISIBLE.replace_all(&s, "");
    let s = ODD_SPACES.replace_all(&s, " ");

    if DOT_LEADER.is_match(&s) {
        // A table-of-contents line ("Chapter 1 ........ 12").
        return String::new();
    }

    let s = SQUARED_UNITS.replace_all(&s, "$1 squared");
    let s = CUBED_UNITS.replace_all(&s, "$1 cubed");
    let s = SUPERSCRIPT_MARKERS.replace_all(&s, "$1");
    let s = BRACKET_REFS.replace_all(&s, "");
    let s = CITATION.replace_all(&s, "");
    let s = TRAILING_MARKS.replace_all(&s, "$1$2");
    let s = URL.replace_all(&s, |c: &Captures| url_to_domain(&c[0]));
    let s = EMOJI.replace_all(&s, "");
    let s = MARKUP_CHARS.replace_all(&s, "");
    let s = EMPTY_PARENS.replace_all(&s, "");
    let s = SPACE_BEFORE_PUNCT.replace_all(&s, "$1");
    let s = MULTI_SPACE.replace_all(&s, " ");
    let s = LEADING_BULLET.replace(s.trim(), "");

    let s = s.trim();
    if s.chars().any(char::is_alphanumeric) {
        s.to_string()
    } else {
        String::new()
    }
}

/// `https://www.example.com/a/b?c, ` -> `example.com, `
fn url_to_domain(url: &str) -> String {
    let trimmed =
        url.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'', '”', '’']);
    let tail = &url[trimmed.len()..];
    let without_scheme = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    let host = without_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.strip_prefix("www.").unwrap_or(host);
    format!("{host}{tail}")
}

/// Clean every block, dropping the ones left empty. Anchors of dropped
/// blocks are carried over to the next surviving block so TOC targets still
/// resolve.
pub fn clean_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    let mut pending: Option<Block> = None;

    for mut block in blocks {
        if block.kind != BlockKind::SceneBreak {
            block.text = clean_text(&block.text);
        }
        let empty = block.kind != BlockKind::SceneBreak && block.text.is_empty();

        if let Some(p) = pending.take() {
            let mut anchors = p.anchors;
            anchors.append(&mut block.anchors);
            block.anchors = anchors;
            if block.page.is_none() {
                block.page = p.page;
            }
            if block.doc.is_none() {
                block.doc = p.doc;
            }
        }

        if empty {
            if block.anchors.is_empty() {
                continue;
            }
            // Only keep the location so the anchors can move forward, unless
            // the next block belongs to another document.
            pending = Some(block);
            continue;
        }

        // Collapse runs of scene breaks and drop leading ones.
        if block.kind == BlockKind::SceneBreak
            && out.last().is_none_or(|b| b.kind == BlockKind::SceneBreak)
        {
            continue;
        }
        out.push(block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_ligatures_and_strips_invisibles() {
        assert_eq!(
            clean_text("the ﬁrst ﬂoor\u{00AD}plan\u{200B}"),
            "the first floorplan"
        );
        assert_eq!(clean_text("non\u{00A0}breaking"), "non breaking");
    }

    #[test]
    fn removes_footnote_markers() {
        assert_eq!(clean_text("as noted.¹ Then"), "as noted. Then");
        assert_eq!(
            clean_text("a claim[12] and more [3, 5–7]."),
            "a claim and more."
        );
        assert_eq!(clean_text("famous[citation needed] words"), "famous words");
        assert_eq!(clean_text("the keeper* said"), "the keeper said");
        assert_eq!(clean_text("an area of 4 m²"), "an area of 4 m squared");
    }

    #[test]
    fn removes_author_year_citations() {
        assert_eq!(clean_text("Light travels (Smith, 2019)."), "Light travels.");
        assert_eq!(
            clean_text("as shown (Smith et al. 2019, p. 4; Doe & Roe 2020a) here"),
            "as shown here"
        );
        // A bare year in parentheses is content, not a citation.
        assert_eq!(clean_text("the storm (1999) hit"), "the storm (1999) hit");
    }

    #[test]
    fn shortens_urls_to_domain() {
        assert_eq!(
            clean_text("Visit https://www.example.org/path?x=1, then"),
            "Visit example.org, then"
        );
        assert_eq!(clean_text("see www.lighthouse.no."), "see lighthouse.no.");
    }

    #[test]
    fn strips_markup_emoji_and_bullets() {
        assert_eq!(clean_text("• **bold** _move_ 🚢 now"), "bold move now");
        assert_eq!(clean_text("Chapter 1 ......... 12"), "");
        assert_eq!(clean_text("* * *"), "");
    }

    #[test]
    fn clean_blocks_moves_anchors_forward() {
        let mut empty = Block::paragraph("[1]");
        empty.anchors = vec!["ch1".into()];
        empty.doc = Some(2);
        let blocks = vec![
            empty,
            Block::paragraph("Text"),
            Block::scene_break(),
            Block::scene_break(),
        ];
        let out = clean_blocks(blocks);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].anchors, vec!["ch1".to_string()]);
        assert_eq!(out[0].doc, Some(2));
    }
}
