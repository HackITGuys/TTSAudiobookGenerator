//! Writes the extraction result to disk:
//!
//! ```text
//! <out>/<book-slug>/book.json
//! <out>/<book-slug>/cover.<ext>
//! <out>/<book-slug>/raw/NNN-<chapter-slug>.md       extracted text, Markdown (input of `refine`)
//! <out>/<book-slug>/text/NNN-<chapter-slug>.md      repaired text written by `refine`
//! <out>/<book-slug>/chapters/NNN-<chapter-slug>.txt what the segments speak
//! <out>/<book-slug>/chapters/NNN-<chapter-slug>.ssml
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::{BlockKind, Book, Chapter};
use crate::segment::spoken_text;
use crate::ssml::chapter_to_ssml;

pub const RAW_DIR: &str = "raw";
pub const TEXT_DIR: &str = "text";
pub const CHAPTERS_DIR: &str = "chapters";

pub fn slugify(s: &str, max_len: usize) -> String {
    let slug = slug::slugify(s);
    let mut cut = slug.chars().take(max_len).collect::<String>();
    while cut.ends_with('-') {
        cut.pop();
    }
    if cut.is_empty() {
        "untitled".into()
    } else {
        cut
    }
}

/// File stem shared by a chapter's raw, text, txt and ssml files.
pub fn chapter_stem(chapter: &Chapter) -> String {
    format!("{:03}-{}", chapter.index, slugify(&chapter.title, 50))
}

/// Human-readable plain text of a chapter as it will be spoken, for review
/// and for TTS engines that take whole files.
pub fn chapter_to_text(chapter: &Chapter) -> String {
    let mut parts = vec![chapter.title.clone()];
    for b in &chapter.blocks {
        parts.push(match b.kind {
            BlockKind::SceneBreak => "* * *".to_string(),
            BlockKind::Heading { .. } | BlockKind::Paragraph => b.text.clone(),
            _ => spoken_text(b.kind, &b.text),
        });
    }
    parts.join("\n\n") + "\n"
}

/// Escape a paragraph line that Markdown would otherwise read as structure.
fn escape_line(text: &str) -> String {
    let structural = text.starts_with(['#', '-', '*', '+', '|', '<', '\\'])
        || text.split_once(['.', ')']).is_some_and(|(n, _)| {
            !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit())
        });
    if structural {
        format!("\\{text}")
    } else {
        text.to_string()
    }
}

/// A chapter as Markdown that [`crate::extract::text::parse`] reads back into
/// the same blocks. Tables and figures are marked with HTML comments so the
/// `refine` stage can turn them into spoken prose.
pub fn chapter_to_markdown(chapter: &Chapter) -> String {
    let mut parts = vec![format!("# {}", chapter.title)];
    for b in &chapter.blocks {
        parts.push(match b.kind {
            BlockKind::Heading { level } => {
                format!(
                    "{} {}",
                    "#".repeat((level as usize + 1).clamp(2, 6)),
                    b.text
                )
            }
            BlockKind::Paragraph => escape_line(&b.text),
            BlockKind::ListItem {
                ordered: true,
                index,
            } => format!("{index}. {}", b.text),
            BlockKind::ListItem { ordered: false, .. } => format!("- {}", b.text),
            BlockKind::SceneBreak => "* * *".to_string(),
            BlockKind::Figure => format!("<!-- figure: {} -->", b.text.replace("--", "–")),
            BlockKind::Table => {
                let mut lines = b.text.lines();
                let caption = lines.next().unwrap_or("").replace("--", "–");
                let mut out = format!("<!-- table: {caption} -->");
                for row in lines {
                    out.push_str(&format!("\n| {row} |"));
                }
                out
            }
        });
    }
    parts.join("\n\n") + "\n"
}

/// Write all files and return the book directory. Fills in the file names
/// on `book` so `book.json` points at them.
pub fn write_book(book: &mut Book, out_root: &Path, write_ssml: bool) -> Result<PathBuf> {
    let title = book
        .metadata
        .title
        .clone()
        .unwrap_or_else(|| "untitled".into());
    let dir = out_root.join(slugify(&title, 60));
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;

    if let Some(cover) = &book.cover {
        let name = format!("cover.{}", cover.extension);
        fs::write(dir.join(&name), &cover.bytes)?;
        book.metadata.cover_file = Some(name);
    }

    let raw_dir = reset_dir(&dir, RAW_DIR)?;
    for chapter in &mut book.chapters {
        let rel = format!("{RAW_DIR}/{}.md", chapter_stem(chapter));
        fs::write(
            raw_dir.join(format!("{}.md", chapter_stem(chapter))),
            chapter_to_markdown(chapter),
        )?;
        chapter.raw_file = Some(rel);
        chapter.refined_file = None;
    }

    write_chapter_files(book, &dir, write_ssml)?;
    Ok(dir)
}

/// Remove and recreate `<dir>/<name>`.
fn reset_dir(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    if path.exists() {
        // Remove stale files from a previous run.
        fs::remove_dir_all(&path).with_context(|| format!("cannot clean {}", path.display()))?;
    }
    fs::create_dir_all(&path).with_context(|| format!("cannot create {}", path.display()))?;
    Ok(path)
}

/// Write `chapters/*.txt|.ssml` and `book.json` for the book's current
/// blocks and segments.
pub fn write_chapter_files(book: &mut Book, dir: &Path, write_ssml: bool) -> Result<()> {
    reset_dir(dir, CHAPTERS_DIR)?;
    let language = book.metadata.language.clone();
    for chapter in &mut book.chapters {
        let stem = chapter_stem(chapter);
        let txt = format!("{CHAPTERS_DIR}/{stem}.txt");
        fs::write(dir.join(&txt), chapter_to_text(chapter))?;
        chapter.text_file = Some(txt);
        chapter.ssml_file = None;
        if write_ssml {
            let ssml = format!("{CHAPTERS_DIR}/{stem}.ssml");
            fs::write(
                dir.join(&ssml),
                chapter_to_ssml(chapter, language.as_deref()),
            )?;
            chapter.ssml_file = Some(ssml);
        }
    }

    let json = serde_json::to_string_pretty(&*book)?;
    fs::write(dir.join("book.json"), json + "\n")?;
    Ok(())
}
