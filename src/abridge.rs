//! Abridged edition of an extracted (and preferably refined) book.
//!
//! `prepare` creates a separate book directory, by default `<book>-abridged`,
//! whose `raw/` holds the full book's best text (`text/`, else `raw/`). The
//! `refine` machinery then runs on it with [`Task::Abridge`], writing the
//! cut text to its `text/`, and `build` makes its `book.json`. The abridged
//! book therefore has its own audio, and `build`, hand edits, `synthesize`
//! and `assemble` work on it like on any other book. The full book is not
//! changed.
//!
//! [`Task::Abridge`]: crate::refine::Task::Abridge

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::chapter_source;
use crate::model::Book;
use crate::output::{RAW_DIR, chapter_stem};
use crate::synthesize::{load_book, save_book};

pub const TITLE_SUFFIX: &str = " (Abridged)";
pub const DEFAULT_PERCENT: u8 = 50;

/// `output/my-book` -> `output/my-book-abridged`.
pub fn default_dir(src_dir: &Path) -> PathBuf {
    let name = src_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".into());
    src_dir.with_file_name(format!("{name}-abridged"))
}

/// Create or update the abridged book directory `dst` from the book in
/// `src`: chapters listed in `exclude` (1-based, as in the full book) are
/// left out, the others are renumbered and their best text is copied to
/// `dst/raw/`. Voice settings and credits of an earlier abridged run are
/// kept so its audio can be reused.
pub fn prepare(src: &Path, dst: &Path, exclude: &[usize]) -> Result<Book> {
    let (full, src_dir) = load_book(src)?;
    if full
        .metadata
        .title
        .as_deref()
        .is_some_and(|t| t.ends_with(TITLE_SUFFIX))
    {
        bail!("{} is already an abridged book", src_dir.display());
    }
    if dst == src_dir {
        bail!("the abridged book needs its own directory");
    }
    let previous = load_book(dst).ok().map(|(b, _)| b);

    let mut book = full.clone();
    book.metadata.title = Some(format!(
        "{}{TITLE_SUFFIX}",
        full.metadata.title.as_deref().unwrap_or("Untitled")
    ));
    book.text_stage = Some("raw".into());
    book.tts = previous.as_ref().and_then(|p| p.tts.clone());
    book.credits = previous.as_ref().and_then(|p| p.credits.clone());

    let raw_dir = dst.join(RAW_DIR);
    if raw_dir.exists() {
        fs::remove_dir_all(&raw_dir)
            .with_context(|| format!("cannot clean {}", raw_dir.display()))?;
    }
    fs::create_dir_all(&raw_dir).with_context(|| format!("cannot create {}", raw_dir.display()))?;

    book.chapters = Vec::new();
    for ch in full.chapters.iter().filter(|c| !exclude.contains(&c.index)) {
        let Some((rel, _)) = chapter_source(&src_dir, ch) else {
            bail!(
                "chapter {} ({}) has no raw/ or text/ Markdown; re-run `extract`",
                ch.index,
                ch.title
            );
        };
        let mut chapter = ch.clone();
        chapter.index = book.chapters.len() + 1;
        chapter.segments.clear();
        chapter.text_file = None;
        chapter.ssml_file = None;
        chapter.refined_file = None;
        let raw_rel = format!("{RAW_DIR}/{}.md", chapter_stem(&chapter));
        fs::copy(src_dir.join(&rel), dst.join(&raw_rel))
            .with_context(|| format!("cannot copy {}", src_dir.join(&rel).display()))?;
        chapter.raw_file = Some(raw_rel);
        book.chapters.push(chapter);
    }
    if book.chapters.is_empty() {
        bail!("every chapter is excluded");
    }

    if let Some(cover) = &full.metadata.cover_file {
        let _ = fs::copy(src_dir.join(cover), dst.join(cover));
    }
    save_book(&book, dst)?;
    Ok(book)
}
