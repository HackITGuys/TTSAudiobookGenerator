//! Writes the extraction result to disk:
//!
//! ```text
//! <out>/<book-slug>/book.json
//! <out>/<book-slug>/cover.<ext>
//! <out>/<book-slug>/chapters/NNN-<chapter-slug>.txt
//! <out>/<book-slug>/chapters/NNN-<chapter-slug>.ssml
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::{BlockKind, Book, Chapter};
use crate::segment::list_item_text;
use crate::ssml::chapter_to_ssml;

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

/// Human-readable plain text of a chapter, for review and for TTS engines
/// that take whole files.
pub fn chapter_to_text(chapter: &Chapter) -> String {
    let mut parts = vec![chapter.title.clone()];
    for b in &chapter.blocks {
        parts.push(match b.kind {
            BlockKind::SceneBreak => "* * *".to_string(),
            BlockKind::ListItem { .. } => list_item_text(b.kind, &b.text),
            _ => b.text.clone(),
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
    let chapters_dir = dir.join("chapters");
    if chapters_dir.exists() {
        // Remove stale chapter files from a previous run.
        fs::remove_dir_all(&chapters_dir)
            .with_context(|| format!("cannot clean {}", chapters_dir.display()))?;
    }
    fs::create_dir_all(&chapters_dir)
        .with_context(|| format!("cannot create {}", chapters_dir.display()))?;

    if let Some(cover) = &book.cover {
        let name = format!("cover.{}", cover.extension);
        fs::write(dir.join(&name), &cover.bytes)?;
        book.metadata.cover_file = Some(name);
    }

    let language = book.metadata.language.clone();
    for chapter in &mut book.chapters {
        let stem = format!("{:03}-{}", chapter.index, slugify(&chapter.title, 50));
        let txt = format!("chapters/{stem}.txt");
        fs::write(dir.join(&txt), chapter_to_text(chapter))?;
        chapter.text_file = Some(txt);
        if write_ssml {
            let ssml = format!("chapters/{stem}.ssml");
            fs::write(
                dir.join(&ssml),
                chapter_to_ssml(chapter, language.as_deref()),
            )?;
            chapter.ssml_file = Some(ssml);
        }
    }

    let json = serde_json::to_string_pretty(&*book)?;
    fs::write(dir.join("book.json"), json + "\n")?;
    Ok(dir)
}
