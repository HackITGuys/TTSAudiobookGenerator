//! Format-specific readers that turn a source file into a `RawDocument`.

pub mod docx;
pub mod epub;
pub mod html;
pub mod pdf;
pub mod text;

use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::model::RawDocument;

pub fn extract(path: &Path) -> Result<RawDocument> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let doc = match ext.as_str() {
        "pdf" => pdf::extract(path),
        "epub" => epub::extract(path),
        "docx" => docx::extract(path),
        "md" | "markdown" | "txt" => text::extract(path),
        other => bail!("unsupported input format '.{other}' (expected pdf, epub, docx, md or txt)"),
    };
    doc.with_context(|| format!("failed to extract text from {}", path.display()))
}

/// Read a zip member as UTF-8 text (lossy, BOM stripped).
pub(crate) fn read_zip_text<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<String> {
    let bytes = read_zip_bytes(zip, name)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.trim_start_matches('\u{FEFF}').to_string())
}

pub(crate) fn read_zip_bytes<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>> {
    let mut file = zip
        .by_name(name)
        .with_context(|| format!("missing archive member '{name}'"))?;
    let mut bytes = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Parse XML, tolerating the DOCTYPEs found in EPUB/OOXML files.
pub(crate) fn parse_xml(text: &str) -> Result<roxmltree::Document<'_>, roxmltree::Error> {
    let opts = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    roxmltree::Document::parse_with_options(text, opts)
}

/// Collapse all whitespace runs into single spaces.
pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split an author string like "A. Smith; B. Jones & C. Doe" into names.
pub(crate) fn split_authors(s: &str) -> Vec<String> {
    s.split([';', '&'])
        .flat_map(|part| part.split(" and "))
        .map(collapse_ws)
        .filter(|a| !a.is_empty())
        .collect()
}

pub(crate) fn non_empty(s: impl AsRef<str>) -> Option<String> {
    let s = collapse_ws(s.as_ref());
    (!s.is_empty()).then_some(s)
}

static SCENE_BREAK_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\s*⁂#~•·∙◆◇❖✦✧-]{1,15}$").unwrap());

/// Text-only scene separators such as `* * *`, `#` or `⁂`.
pub(crate) fn is_scene_break_text(text: &str) -> bool {
    SCENE_BREAK_TEXT.is_match(text.trim())
}
