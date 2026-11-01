//! SSML rendering of a chapter, for TTS engines that support it.
//!
//! Mistral's TTS does not accept SSML; it uses the plain-text segments
//! instead. The break lengths here mirror the segment pauses so both
//! renderings sound the same.

use std::fmt::Write;

use crate::model::{BlockKind, Chapter};
use crate::segment::{
    HEADING_PAUSE_MS, LIST_ITEM_PAUSE_MS, SCENE_BREAK_PAUSE_MS, TITLE_PAUSE_MS, list_item_text,
};
use crate::sentences::split_sentences;

pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn sentences_xml(text: &str) -> String {
    split_sentences(text)
        .into_iter()
        .map(|s| format!("<s>{}</s>", escape_xml(s)))
        .collect()
}

/// Render a chapter as a standalone `<speak>` document.
pub fn chapter_to_ssml(chapter: &Chapter, language: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<speak version=\"1.1\" xmlns=\"http://www.w3.org/2001/10/synthesis\"");
    if let Some(lang) = language {
        let _ = write!(out, " xml:lang=\"{}\"", escape_xml(lang));
    }
    out.push_str(">\n");

    let _ = writeln!(
        out,
        "  <p><prosody rate=\"90%\">{}</prosody></p>\n  <break time=\"{TITLE_PAUSE_MS}ms\"/>",
        escape_xml(&chapter.title)
    );

    for block in &chapter.blocks {
        match block.kind {
            BlockKind::Heading { .. } => {
                let _ = writeln!(
                    out,
                    "  <break time=\"{HEADING_PAUSE_MS}ms\"/>\n  <p><prosody rate=\"95%\">{}</prosody></p>\n  <break time=\"{HEADING_PAUSE_MS}ms\"/>",
                    escape_xml(&block.text)
                );
            }
            BlockKind::Paragraph => {
                let _ = writeln!(out, "  <p>{}</p>", sentences_xml(&block.text));
            }
            BlockKind::ListItem { .. } => {
                let _ = writeln!(
                    out,
                    "  <p>{}</p>\n  <break time=\"{LIST_ITEM_PAUSE_MS}ms\"/>",
                    sentences_xml(&list_item_text(block.kind, &block.text))
                );
            }
            BlockKind::SceneBreak => {
                let _ = writeln!(out, "  <break time=\"{SCENE_BREAK_PAUSE_MS}ms\"/>");
            }
        }
    }
    out.push_str("</speak>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Block;

    #[test]
    fn renders_escaped_well_formed_ssml() {
        let chapter = Chapter::new(
            1,
            "Salt & Light".into(),
            vec![
                Block::paragraph("He said <no>. Then \"yes\"."),
                Block::scene_break(),
                Block::heading(3, "Night"),
            ],
        );
        let ssml = chapter_to_ssml(&chapter, Some("en"));
        assert!(ssml.contains("<prosody rate=\"90%\">Salt &amp; Light</prosody>"));
        assert!(ssml.contains("<p><s>He said &lt;no&gt;.</s><s>Then &quot;yes&quot;.</s></p>"));
        assert!(ssml.contains("<break time=\"2000ms\"/>"));
        let doc = roxmltree::Document::parse(&ssml).expect("valid XML");
        assert_eq!(doc.root_element().tag_name().name(), "speak");
    }
}
