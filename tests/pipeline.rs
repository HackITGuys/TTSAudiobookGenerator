//! End-to-end checks of the text pipeline on the same sample book rendered
//! as Markdown, EPUB, DOCX and PDF (see `tests/fixtures/generate.ps1`).

use std::path::PathBuf;

use tts_audiobook_generator::model::{BlockKind, Book};
use tts_audiobook_generator::output::{chapter_to_text, write_book};
use tts_audiobook_generator::{ExtractOptions, extract_book};

const EXPECTED_TITLES: [&str; 4] = [
    "Part One — Chapter 1: The Fog",
    "Chapter 2: The Storm",
    "Part Two — Chapter 3: The Ship",
    "Chapter 4: The Return",
];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn extract(name: &str, opts: &ExtractOptions) -> Book {
    extract_book(&fixture(name), opts).unwrap_or_else(|e| panic!("{name}: {e:#}"))
}

fn assert_sample_book(book: &Book, name: &str) {
    let m = &book.metadata;
    assert_eq!(
        m.title.as_deref(),
        Some("The Keeper of Skarvøy Light"),
        "{name}: title"
    );
    assert_eq!(
        m.authors,
        vec!["Ingrid Solberg".to_string()],
        "{name}: authors"
    );

    let titles: Vec<&str> = book.chapters.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, EXPECTED_TITLES, "{name}: chapter titles");
    for (i, c) in book.chapters.iter().enumerate() {
        assert_eq!(c.index, i + 1);
        assert!(
            c.word_count > 200,
            "{name}: chapter {} too short ({} words)",
            c.index,
            c.word_count
        );
    }

    // Chapter 1 keeps its section heading, numbered list and scene break.
    let ch1 = &book.chapters[0];
    assert!(
        ch1.blocks
            .iter()
            .any(|b| b.is_heading() && b.text == "The Logbook"),
        "{name}: section heading missing"
    );
    let list: Vec<(u32, &str)> = ch1
        .blocks
        .iter()
        .filter_map(|b| match b.kind {
            BlockKind::ListItem {
                ordered: true,
                index,
            } => Some((index, b.text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(list.len(), 3, "{name}: numbered list");
    assert_eq!(
        list[0],
        (1, "Trim the wick and clean the soot from the glass.")
    );
    assert!(
        ch1.blocks.iter().any(|b| b.kind == BlockKind::SceneBreak),
        "{name}: scene break missing"
    );
    let bullets = book.chapters[1]
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::ListItem { ordered: false, .. }))
        .count();
    assert_eq!(bullets, 3, "{name}: bullet list");

    let all_text: String = book.chapters.iter().map(chapter_to_text).collect();
    for unwanted in [
        "registered in the coastal archive", // footnote 1 body
        "this detail is disputed",           // footnote 2 body
        "Hansen, 1923",                      // author-year citation
        "Berg & Lund",                       // author-year citation
        "[2]",                               // bracketed reference
        "https://",                          // URL reduced to its domain
        "ﬁ",                                 // ligature glyph
    ] {
        assert!(
            !all_text.contains(unwanted),
            "{name}: found {unwanted:?} in narration text"
        );
    }
    // Markdown keeps straight quotes; pandoc's other outputs use curly ones.
    let plain_quotes = all_text.replace(['“', '”'], "\"").replace(['‘', '’'], "'");
    for wanted in [
        "online, at example.org, but",
        "The office at the top",
        "Mr. Moen did not mind.",
        "\"You could not have known,\" Dr. Aune said at last.",
    ] {
        assert!(plain_quotes.contains(wanted), "{name}: missing {wanted:?}");
    }

    for c in &book.chapters {
        assert_eq!(
            c.segments[0].text,
            format!("{}.", c.title),
            "{name}: title segment"
        );
        assert!(
            c.segments.iter().all(|s| s.chars <= 4000
                && s.chars == s.text.chars().count()
                && !s.text.is_empty())
        );
        // Segments also speak the numbers of ordered list items.
        let numbers = c
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::ListItem { ordered: true, .. }))
            .count();
        let seg_words: usize = c.segments.iter().map(|s| s.words).sum();
        assert_eq!(
            seg_words,
            c.word_count + numbers,
            "{name}: segments must cover the whole chapter"
        );
    }
}

#[test]
fn markdown_sample() {
    assert_sample_book(&extract("sample.md", &ExtractOptions::default()), "md");
}

#[test]
fn epub_sample() {
    let book = extract("sample.epub", &ExtractOptions::default());
    assert_sample_book(&book, "epub");
    assert_eq!(book.metadata.language.as_deref(), Some("en"));
    assert_eq!(
        book.metadata.publisher.as_deref(),
        Some("HackITGuys Test Press")
    );
    assert_eq!(book.metadata.identifier.as_deref(), Some("9780000000002"));
}

#[test]
fn epub_sample_without_toc() {
    let opts = ExtractOptions {
        ignore_toc: true,
        ..Default::default()
    };
    assert_sample_book(&extract("sample.epub", &opts), "epub/headings");
}

#[test]
fn docx_sample() {
    let book = extract("sample.docx", &ExtractOptions::default());
    assert_sample_book(&book, "docx");
    assert_eq!(book.metadata.subtitle.as_deref(), Some("A Sample Novella"));
}

#[test]
fn pdf_sample_with_outline() {
    assert_sample_book(&extract("sample.pdf", &ExtractOptions::default()), "pdf");
}

#[test]
fn pdf_sample_from_font_sizes() {
    let opts = ExtractOptions {
        ignore_toc: true,
        ..Default::default()
    };
    assert_sample_book(&extract("sample.pdf", &opts), "pdf/headings");
}

#[test]
fn smaller_segments_respect_char_limit() {
    let opts = ExtractOptions {
        max_segment_chars: 300,
        ..Default::default()
    };
    let book = extract("sample.epub", &opts);
    for c in &book.chapters {
        assert!(c.segments.iter().all(|s| s.chars <= 300));
        assert!(c.segments.len() > 5);
    }
}

#[test]
fn writes_output_files() {
    let tmp = tempfile::tempdir().unwrap();
    let mut book = extract("sample.docx", &ExtractOptions::default());
    let dir = write_book(&mut book, tmp.path(), true).unwrap();

    assert_eq!(dir.file_name().unwrap(), "the-keeper-of-skarvoy-light");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("book.json")).unwrap()).unwrap();
    assert_eq!(json["metadata"]["title"], "The Keeper of Skarvøy Light");
    assert_eq!(json["chapters"].as_array().unwrap().len(), 4);
    assert_eq!(
        json["chapters"][0]["text_file"],
        "chapters/001-part-one-chapter-1-the-fog.txt"
    );

    for chapter in &book.chapters {
        let txt = std::fs::read_to_string(dir.join(chapter.text_file.as_ref().unwrap())).unwrap();
        assert!(txt.starts_with(&chapter.title));
        let ssml = std::fs::read_to_string(dir.join(chapter.ssml_file.as_ref().unwrap())).unwrap();
        let doc = roxmltree::Document::parse(&ssml).expect("SSML must be well-formed XML");
        assert_eq!(doc.root_element().tag_name().name(), "speak");
    }
}

#[test]
fn sanitises_codes_before_segmenting() {
    let tmp = tempfile::tempdir().unwrap();
    let md = tmp.path().join("tech.md");
    std::fs::write(
        &md,
        "# Setup\n\nVerify the download against 3f786850e387550fdab836ed7e6dc881de23001b. \
         Then export TOKEN=a8Fk29xLq0Zm7Q and run the installer.\n\n\
         # Usage\n\nStart the service on 192.168.100.200 and wait.\n",
    )
    .unwrap();
    let book = extract_book(&md, &ExtractOptions::default()).unwrap();
    let spoken: String = book
        .chapters
        .iter()
        .flat_map(|c| &c.segments)
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(spoken.contains("against long non-textual string ending in 2 3 0 0 1 b."));
    assert!(spoken.contains("long non-textual string ending in q 0 Z m 7 Q and run"));
    assert!(spoken.contains("192.168.100.200"));
    assert!(!spoken.contains("3f786850"));

    let raw = extract_book(
        &md,
        &ExtractOptions {
            sanitize: false,
            ..Default::default()
        },
    )
    .unwrap();
    let raw_text: String = raw
        .chapters
        .iter()
        .flat_map(|c| &c.segments)
        .map(|s| s.text.as_str())
        .collect();
    assert!(raw_text.contains("3f786850e387550fdab836ed7e6dc881de23001b"));
}
