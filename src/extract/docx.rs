//! DOCX reader: `word/document.xml` paragraphs with heading levels resolved
//! through `styles.xml` (so localised style names work), lists through
//! `numbering.xml`, and metadata from `docProps/core.xml`.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use anyhow::Result;
use roxmltree::Node;

use super::{is_scene_break_text, non_empty, parse_xml, read_zip_text, split_authors};
use crate::model::{Block, BlockKind, BookMetadata, RawDocument};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

#[derive(Debug, Clone, Default)]
struct Style {
    name: String,
    outline_level: Option<u8>,
    based_on: Option<String>,
    num_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ParaRole {
    Heading(u8),
    Title,
    Subtitle,
    Toc,
    List,
    Body,
}

struct Styles(HashMap<String, Style>);

impl Styles {
    fn parse(xml: Option<&str>) -> Self {
        let mut map = HashMap::new();
        let Some(doc) = xml.and_then(|x| parse_xml(x).ok()) else {
            return Self(map);
        };
        for s in doc.descendants().filter(|n| is_w(n, "style")) {
            let Some(id) = w_attr(&s, "styleId") else {
                continue;
            };
            let child_val = |name: &str| {
                child(&s, name)
                    .and_then(|c| w_attr(&c, "val"))
                    .map(str::to_string)
            };
            let ppr = child(&s, "pPr");
            map.insert(
                id.to_string(),
                Style {
                    name: child_val("name").unwrap_or_default(),
                    outline_level: ppr
                        .and_then(|p| child(&p, "outlineLvl"))
                        .and_then(|o| w_attr(&o, "val"))
                        .and_then(|v| v.parse().ok()),
                    based_on: child_val("basedOn"),
                    num_id: ppr
                        .and_then(|p| child(&p, "numPr"))
                        .and_then(|n| child(&n, "numId"))
                        .and_then(|n| w_attr(&n, "val"))
                        .map(str::to_string),
                },
            );
        }
        Self(map)
    }

    /// Role of a paragraph style, following `basedOn` inheritance.
    fn role(&self, style_id: &str) -> ParaRole {
        let mut id = Some(style_id.to_string());
        for _ in 0..10 {
            let Some(style) = id.as_deref().and_then(|i| self.0.get(i)) else {
                break;
            };
            let name = style.name.to_ascii_lowercase();
            if let Some(n) = name
                .strip_prefix("heading ")
                .and_then(|n| n.trim().parse::<u8>().ok())
            {
                return ParaRole::Heading(n.clamp(1, 6));
            }
            match name.as_str() {
                "title" => return ParaRole::Title,
                "subtitle" => return ParaRole::Subtitle,
                "toc heading" => return ParaRole::Toc,
                _ if name.starts_with("toc ") => return ParaRole::Toc,
                _ if name.starts_with("list bullet")
                    || name.starts_with("list number")
                    || style.num_id.is_some() =>
                {
                    return ParaRole::List;
                }
                _ => {}
            }
            if let Some(lvl) = style.outline_level.filter(|l| *l < 9) {
                return ParaRole::Heading((lvl + 1).min(6));
            }
            id = style.based_on.clone();
        }
        ParaRole::Body
    }

    fn style_num_id(&self, style_id: &str) -> Option<&str> {
        self.0.get(style_id).and_then(|s| s.num_id.as_deref())
    }
}

/// numId -> ilvl -> ordered?
struct Numbering(HashMap<String, HashMap<String, bool>>);

impl Numbering {
    fn parse(xml: Option<&str>) -> Self {
        let mut out = HashMap::new();
        let Some(doc) = xml.and_then(|x| parse_xml(x).ok()) else {
            return Self(out);
        };
        let mut abstracts: HashMap<&str, HashMap<String, bool>> = HashMap::new();
        for a in doc.descendants().filter(|n| is_w(n, "abstractNum")) {
            let Some(id) = w_attr(&a, "abstractNumId") else {
                continue;
            };
            let levels = a
                .children()
                .filter(|n| is_w(n, "lvl"))
                .filter_map(|lvl| {
                    let ilvl = w_attr(&lvl, "ilvl")?.to_string();
                    let fmt = child(&lvl, "numFmt")
                        .and_then(|f| w_attr(&f, "val"))
                        .unwrap_or("bullet");
                    Some((ilvl, !matches!(fmt, "bullet" | "none")))
                })
                .collect();
            abstracts.insert(id, levels);
        }
        for n in doc.descendants().filter(|n| is_w(n, "num")) {
            let (Some(num_id), Some(abs)) = (
                w_attr(&n, "numId"),
                child(&n, "abstractNumId").and_then(|a| w_attr(&a, "val")),
            ) else {
                continue;
            };
            if let Some(levels) = abstracts.get(abs) {
                out.insert(num_id.to_string(), levels.clone());
            }
        }
        Self(out)
    }

    fn is_ordered(&self, num_id: &str, ilvl: &str) -> bool {
        self.0
            .get(num_id)
            .and_then(|l| l.get(ilvl))
            .copied()
            .unwrap_or(false)
    }
}

fn is_w(n: &Node, name: &str) -> bool {
    n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some(W)
}

fn child<'a, 'i>(n: &Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| is_w(c, name))
}

fn w_attr<'a>(n: &Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.attribute((W, name))
}

pub fn extract(path: &Path) -> Result<RawDocument> {
    let mut zip = zip::ZipArchive::new(File::open(path)?)?;
    let document = read_zip_text(&mut zip, "word/document.xml")?;
    let styles = Styles::parse(read_zip_text(&mut zip, "word/styles.xml").ok().as_deref());
    let numbering = Numbering::parse(
        read_zip_text(&mut zip, "word/numbering.xml")
            .ok()
            .as_deref(),
    );
    let mut metadata =
        read_core_props(read_zip_text(&mut zip, "docProps/core.xml").ok().as_deref());
    metadata.source_format = "docx".into();

    let doc = parse_xml(&document)?;
    let body = doc
        .descendants()
        .find(|n| is_w(n, "body"))
        .ok_or_else(|| anyhow::anyhow!("document.xml has no body"))?;

    let mut reader = BodyReader {
        styles: &styles,
        numbering: &numbering,
        blocks: Vec::new(),
        title: None,
        subtitle: None,
        list_counters: HashMap::new(),
    };
    reader.container(body);

    if metadata.title.is_none() {
        metadata.title = reader.title.take();
    }
    if metadata.subtitle.is_none() {
        metadata.subtitle = reader.subtitle.take();
    }
    Ok(RawDocument {
        metadata,
        blocks: reader.blocks,
        toc: Vec::new(),
        cover: None,
    })
}

struct BodyReader<'s> {
    styles: &'s Styles,
    numbering: &'s Numbering,
    blocks: Vec<Block>,
    title: Option<String>,
    subtitle: Option<String>,
    /// (numId, ilvl) -> running item number
    list_counters: HashMap<(String, String), u32>,
}

impl BodyReader<'_> {
    /// Walk block-level content: paragraphs, tables and content controls.
    fn container(&mut self, node: Node) {
        for n in node.children().filter(Node::is_element) {
            match n.tag_name().name() {
                "p" if is_w(&n, "p") => self.paragraph(n),
                "tbl" | "tr" | "tc" | "sdtContent" | "customXml" => self.container(n),
                "sdt" if !is_toc_sdt(&n) => self.container(n),
                _ => {}
            }
        }
    }

    fn paragraph(&mut self, p: Node) {
        let ppr = child(&p, "pPr");
        let style_id = ppr
            .and_then(|pp| child(&pp, "pStyle"))
            .and_then(|s| w_attr(&s, "val"))
            .unwrap_or("");
        let mut role = self.styles.role(style_id);
        if let Some(lvl) = ppr
            .and_then(|pp| child(&pp, "outlineLvl"))
            .and_then(|o| w_attr(&o, "val"))
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|l| *l < 9)
        {
            role = ParaRole::Heading((lvl + 1).min(6));
        }

        let num_pr = ppr.and_then(|pp| child(&pp, "numPr"));
        let num_id = num_pr
            .and_then(|n| child(&n, "numId"))
            .and_then(|n| w_attr(&n, "val"))
            .or_else(|| self.styles.style_num_id(style_id))
            .filter(|id| *id != "0");
        let ilvl = num_pr
            .and_then(|n| child(&n, "ilvl"))
            .and_then(|n| w_attr(&n, "val"))
            .unwrap_or("0");

        let mut text = String::new();
        collect_text(p, &mut text);
        let text = super::collapse_ws(&text);
        if (text.is_empty() && is_horizontal_rule(&p))
            || (!text.is_empty() && is_scene_break_text(&text))
        {
            self.blocks.push(Block::scene_break());
            return;
        }
        if text.is_empty() {
            return;
        }

        let kind = match role {
            ParaRole::Title => {
                self.title.get_or_insert(text);
                return;
            }
            ParaRole::Subtitle => {
                self.subtitle.get_or_insert(text);
                return;
            }
            ParaRole::Toc => return,
            ParaRole::Heading(level) => BlockKind::Heading { level },
            ParaRole::List | ParaRole::Body => match num_id {
                Some(id) => {
                    let ordered = self.numbering.is_ordered(id, ilvl);
                    let counter = self
                        .list_counters
                        .entry((id.to_string(), ilvl.to_string()))
                        .or_insert(0);
                    *counter += 1;
                    BlockKind::ListItem {
                        ordered,
                        index: *counter,
                    }
                }
                None if role == ParaRole::List => BlockKind::ListItem {
                    ordered: false,
                    index: 0,
                },
                None => BlockKind::Paragraph,
            },
        };
        if !matches!(kind, BlockKind::ListItem { .. }) {
            // A list's numbering restarts once a non-list paragraph intervenes.
            self.list_counters.clear();
        }
        self.blocks.push(Block::new(kind, text));
    }
}

/// A VML horizontal rule (`<v:rect o:hr="t"/>`), as written by Word and pandoc.
fn is_horizontal_rule(p: &Node) -> bool {
    p.descendants()
        .any(|n| n.attributes().any(|a| a.name() == "hr" && a.value() == "t"))
}

/// Word's generated table of contents lives in a content control.
fn is_toc_sdt(sdt: &Node) -> bool {
    sdt.descendants()
        .filter(|n| is_w(n, "docPartGallery"))
        .any(|n| w_attr(&n, "val").is_some_and(|v| v.contains("Table of Contents")))
}

/// Collect the visible text of a paragraph, skipping deleted text, field
/// instructions, note references and drawings.
fn collect_text(node: Node, out: &mut String) {
    for n in node.children() {
        if !n.is_element() {
            continue;
        }
        let tag = n.tag_name();
        if tag.namespace() != Some(W) {
            // mc:AlternateContent, drawings etc. duplicate or carry no text.
            continue;
        }
        match tag.name() {
            "t" => out.push_str(n.text().unwrap_or("")),
            "tab" | "br" | "cr" => out.push(' '),
            "noBreakHyphen" => out.push('-'),
            "del" | "delText" | "instrText" | "footnoteReference" | "endnoteReference"
            | "commentReference" | "drawing" | "pict" | "object" | "pPr" | "rPr" => {}
            _ => collect_text(n, out),
        }
    }
}

fn read_core_props(xml: Option<&str>) -> BookMetadata {
    let mut meta = BookMetadata::default();
    let Some(doc) = xml.and_then(|x| parse_xml(x).ok()) else {
        return meta;
    };
    for n in doc.root_element().children().filter(Node::is_element) {
        let value = n.text().unwrap_or("");
        match n.tag_name().name() {
            "title" => meta.title = non_empty(value),
            "creator" => meta.authors = split_authors(value),
            "subject" => meta.subjects.extend(non_empty(value)),
            "keywords" => meta
                .subjects
                .extend(value.split([',', ';']).filter_map(non_empty)),
            "description" => meta.description = non_empty(value),
            "language" => meta.language = non_empty(value),
            _ => {}
        }
    }
    meta
}
