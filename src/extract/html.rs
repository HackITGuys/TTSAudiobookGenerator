//! (X)HTML to blocks, used for EPUB content documents.
//!
//! Parsed with html5ever (via scraper) so sloppy markup and HTML entities are
//! handled. Footnote references and footnote bodies are dropped here, where
//! the markup still says what they are.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use scraper::{ElementRef, Html, Node, Selector};

use super::{collapse_ws, is_scene_break_text};
use crate::model::{Block, BlockKind};

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Element names whose content is never narrated.
const SKIP_ELEMENTS: &[&str] = &[
    "head", "script", "style", "title", "svg", "math", "img", "figure", "nav", "noscript",
    "iframe", "object", "video", "audio", "button", "form",
];

/// `epub:type` / `role` / `class` tokens marking notes and page markers.
const NOTE_TOKENS: &[&str] = &[
    "footnote",
    "footnotes",
    "endnote",
    "endnotes",
    "rearnote",
    "rearnotes",
    "doc-footnote",
    "doc-endnote",
    "doc-endnotes",
    "noteref",
    "doc-noteref",
    "footnote-ref",
    "pagebreak",
    "doc-pagebreak",
];

const BLOCK_ELEMENTS: &[&str] = &[
    "p",
    "div",
    "blockquote",
    "section",
    "article",
    "header",
    "footer",
    "main",
    "aside",
    "address",
    "pre",
    "dl",
    "dt",
    "dd",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "caption",
    "center",
    "body",
    "hgroup",
    "figcaption",
    "details",
    "summary",
];

static SELF_CLOSING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<([A-Za-z][\w:.-]*)((?:\s[^<>]*?)?)\s*/>").unwrap());
static FOOTNOTE_MARK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\d\s*†‡§,\-–\[\]()]+$").unwrap());
static BODY: LazyLock<Selector> = LazyLock::new(|| Selector::parse("body").unwrap());

/// XHTML allows `<div/>`; the HTML parser would treat it as an open tag and
/// swallow the following content into it.
fn expand_self_closing(xhtml: &str) -> String {
    SELF_CLOSING
        .replace_all(xhtml, |c: &Captures| {
            let tag = &c[1];
            if VOID_ELEMENTS.contains(&tag.to_ascii_lowercase().as_str()) {
                c[0].to_string()
            } else {
                format!("<{tag}{}></{tag}>", &c[2])
            }
        })
        .into_owned()
}

pub fn xhtml_to_blocks(xhtml: &str, doc: Option<usize>) -> Vec<Block> {
    let html = Html::parse_document(&expand_self_closing(xhtml));
    let root = html
        .select(&BODY)
        .next()
        .unwrap_or_else(|| html.root_element());

    let mut b = Builder {
        blocks: Vec::new(),
        buf: String::new(),
        kinds: vec![BlockKind::Paragraph],
        lists: Vec::new(),
        anchors: Vec::new(),
        doc,
    };
    b.walk(root);
    b.flush();
    b.blocks
}

struct ListCtx {
    ordered: bool,
    next: u32,
}

struct Builder {
    blocks: Vec<Block>,
    buf: String,
    /// Kind of the innermost open block element.
    kinds: Vec<BlockKind>,
    lists: Vec<ListCtx>,
    anchors: Vec<String>,
    doc: Option<usize>,
}

impl Builder {
    fn flush(&mut self) {
        let text = collapse_ws(&self.buf);
        self.buf.clear();
        if text.is_empty() {
            return;
        }
        let kind = if is_scene_break_text(&text) {
            BlockKind::SceneBreak
        } else {
            *self.kinds.last().unwrap_or(&BlockKind::Paragraph)
        };
        let text = if kind == BlockKind::SceneBreak {
            String::new()
        } else {
            text
        };
        let mut block = Block::new(kind, text);
        block.doc = self.doc;
        block.anchors = std::mem::take(&mut self.anchors);
        self.blocks.push(block);
    }

    fn push_block(&mut self, block: Block) {
        self.flush();
        let mut block = block;
        block.doc = self.doc;
        block.anchors = std::mem::take(&mut self.anchors);
        self.blocks.push(block);
    }

    fn walk(&mut self, el: ElementRef) {
        let name = el.value().name().to_ascii_lowercase();
        if SKIP_ELEMENTS.contains(&name.as_str()) || is_note(&el) || is_footnote_sup(&name, &el) {
            return;
        }

        let heading = name
            .strip_prefix('h')
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|n| (1..=6).contains(n));

        let block_kind = if let Some(level) = heading {
            Some(BlockKind::Heading { level })
        } else if name == "li" {
            let list = self.lists.last_mut();
            let (ordered, index) = match list {
                Some(l) => {
                    let i = l.next;
                    l.next += 1;
                    (l.ordered, i)
                }
                None => (false, 0),
            };
            Some(BlockKind::ListItem { ordered, index })
        } else if BLOCK_ELEMENTS.contains(&name.as_str()) {
            // Paragraphs inside a list item or heading keep that role.
            Some(match self.kinds.last() {
                Some(k @ (BlockKind::ListItem { .. } | BlockKind::Heading { .. })) => *k,
                _ => BlockKind::Paragraph,
            })
        } else {
            None
        };

        match name.as_str() {
            "hr" => {
                if let Some(id) = el.value().id() {
                    self.anchors.push(id.to_string());
                }
                self.push_block(Block::scene_break());
                return;
            }
            "br" => {
                self.buf.push(' ');
                return;
            }
            "ol" | "ul" => {
                self.flush();
                let start = el
                    .value()
                    .attr("start")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                self.lists.push(ListCtx {
                    ordered: name == "ol",
                    next: start,
                });
            }
            _ => {}
        }

        if let Some(kind) = block_kind {
            self.flush();
            self.kinds.push(kind);
        }
        if let Some(id) = el.value().id() {
            self.anchors.push(id.to_string());
        }

        for child in el.children() {
            match child.value() {
                Node::Text(t) => self.buf.push_str(t),
                Node::Element(_) => {
                    if let Some(c) = ElementRef::wrap(child) {
                        self.walk(c);
                    }
                }
                _ => {}
            }
        }

        if block_kind.is_some() {
            self.flush();
            self.kinds.pop();
        }
        if matches!(name.as_str(), "ol" | "ul") {
            self.lists.pop();
        }
    }
}

fn is_note(el: &ElementRef) -> bool {
    el.value().attrs().any(|(name, value)| {
        matches!(name, "epub:type" | "type" | "role" | "class")
            && value
                .split_whitespace()
                .any(|t| NOTE_TOKENS.contains(&t.to_ascii_lowercase().as_str()))
    })
}

/// `<sup>12</sup>` or `<sup><a href="#n12">12</a></sup>` is a note marker;
/// `1<sup>st</sup>` is not.
fn is_footnote_sup(name: &str, el: &ElementRef) -> bool {
    if name != "sup" {
        return false;
    }
    let text: String = el.text().collect();
    FOOTNOTE_MARK.is_match(text.trim())
        || el
            .children()
            .filter_map(ElementRef::wrap)
            .any(|c| c.value().name() == "a")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_structure_and_drops_notes() {
        let xhtml = r##"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Ch 1</title><style>p{}</style></head>
<body>
<section id="ch1" epub:type="chapter">
<h2 id="h-1">Chapter&#160;1<br/>The Lighthouse</h2>
<p>It was the 1<sup>st</sup> night&nbsp;of fog.<a epub:type="noteref" href="#n1"><sup>1</sup></a></p>
<div/>
<p>Second <em>paragraph</em> here.<sup>2</sup></p>
<p class="center">* * *</p>
<ol><li>Light the lamp.</li><li><p>Wind the clock.</p></li></ol>
<hr/>
<ul><li>Oil</li></ul>
<figure><img src="x.jpg"/><figcaption>A lighthouse</figcaption></figure>
<aside epub:type="footnote" id="n1"><p>The note text.</p></aside>
</section>
</body></html>"##;
        let blocks = xhtml_to_blocks(xhtml, Some(3));
        let got: Vec<(BlockKind, &str)> =
            blocks.iter().map(|b| (b.kind, b.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (BlockKind::Heading { level: 2 }, "Chapter 1 The Lighthouse"),
                (BlockKind::Paragraph, "It was the 1st night of fog."),
                (BlockKind::Paragraph, "Second paragraph here."),
                (BlockKind::SceneBreak, ""),
                (
                    BlockKind::ListItem {
                        ordered: true,
                        index: 1
                    },
                    "Light the lamp."
                ),
                (
                    BlockKind::ListItem {
                        ordered: true,
                        index: 2
                    },
                    "Wind the clock."
                ),
                (BlockKind::SceneBreak, ""),
                (
                    BlockKind::ListItem {
                        ordered: false,
                        index: 1
                    },
                    "Oil"
                ),
            ]
        );
        assert_eq!(
            blocks[0].anchors,
            vec!["ch1".to_string(), "h-1".to_string()]
        );
        assert!(blocks.iter().all(|b| b.doc == Some(3)));
    }
}
