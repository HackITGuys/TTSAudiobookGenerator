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
    assert!(spoken.contains("against a SHA-1 hash."));
    assert!(spoken.contains("TOKEN=a random code and run"));
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

// ------------------------------------------------------------ refine / build

use std::sync::Mutex;

use tts_audiobook_generator::llm::{ChatBackend, ChatError, ChatMessage};
use tts_audiobook_generator::refine::{RefineOptions, Task, refine};
use tts_audiobook_generator::synthesize::{load_book, save_book};
use tts_audiobook_generator::{BuildOptions, build_book};

const TECH_BOOK: &str = "---\ntitle: Tech Notes\nauthor: A. Writer\n---\n\n\
# Chapter 1: Setup\n\n\
Install the tool and check the download. The steps below are the same on every system, \
and the next section explains each of them in turn.\n\n\
<!-- table: Table 1-1: Supported Systems -->\n| System | Notes |\n| Linux | Best |\n\n\
<!-- figure: Figure 1-1: The setup screen -->\n\n\
## Checking\n\n\
Compare the result with the published value before you run anything.\n\n\
# Chapter 2: Usage\n\n\
Start the service and wait until it reports that it is ready to accept requests.\n";

/// Answers like a well-behaved model: tables and figures become prose, the
/// rest is returned unchanged.
struct FakeModel {
    calls: Mutex<usize>,
    /// Answer with commentary to trigger the validation fallback.
    chatty: bool,
}

impl ChatBackend for FakeModel {
    fn chat(&self, _model: &str, messages: &[ChatMessage]) -> Result<String, ChatError> {
        *self.calls.lock().unwrap() += 1;
        let user = &messages[1].content;
        let input = user
            .rsplit("TEXT TO REPAIR:\n<<<\n")
            .next()
            .unwrap()
            .trim_end_matches(">>>")
            .trim();
        let mut out = Vec::new();
        for block in input.split("\n\n") {
            if let Some(rest) = block.strip_prefix("<!-- table: ") {
                let caption = rest.lines().next().unwrap().trim_end_matches(" -->");
                out.push(format!(
                    "{caption}, lists one system, Linux, which works best."
                ));
            } else if let Some(rest) = block.strip_prefix("<!-- figure: ") {
                out.push(format!(
                    "{} shows the first screen.",
                    rest.trim_end_matches(" -->")
                ));
            } else {
                out.push(block.to_string());
            }
        }
        let answer = out.join("\n\n");
        Ok(if self.chatty {
            format!("Here is the repaired text:\n\n{answer}")
        } else {
            answer
        })
    }
}

fn refine_opts() -> RefineOptions {
    RefineOptions {
        task: Task::Repair,
        model: "fake".into(),
        concurrency: 2,
        force: false,
        chapters: None,
        chunk_chars: 6000,
    }
}

fn extracted_tech_book(tmp: &std::path::Path) -> PathBuf {
    let md = tmp.join("tech.md");
    std::fs::write(&md, TECH_BOOK).unwrap();
    let mut book = extract_book(&md, &ExtractOptions::default()).unwrap();
    write_book(&mut book, &tmp.join("out"), true).unwrap()
}

#[test]
fn build_from_raw_markdown_reproduces_extract() {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["sample.docx", "sample.epub", "sample.pdf"] {
        let mut book = extract(name, &ExtractOptions::default());
        let dir = write_book(&mut book, &tmp.path().join(name), true).unwrap();
        assert!(dir.join("raw").is_dir());
        let rebuilt = build_book(&dir, &BuildOptions::default()).unwrap();
        assert_eq!(rebuilt.text_stage.as_deref(), Some("raw"));
        let texts = |b: &Book| -> Vec<Vec<String>> {
            b.chapters
                .iter()
                .map(|c| c.segments.iter().map(|s| s.text.clone()).collect())
                .collect()
        };
        assert_eq!(texts(&rebuilt), texts(&book), "{name}: raw/ round trip");
    }
}

#[test]
fn refine_writes_text_caches_answers_and_keeps_hand_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = extracted_tech_book(tmp.path());

    // Without refine, tables are only referenced and figures read as captions.
    let raw = build_book(&dir, &BuildOptions::default()).unwrap();
    let raw_text: String = raw.chapters[0]
        .segments
        .iter()
        .map(|s| s.text.clone())
        .collect();
    assert!(raw_text.contains("Table 1-1, Supported Systems, is shown in the book."));
    assert!(raw_text.contains("Figure 1-1: The setup screen."));

    let model = FakeModel {
        calls: Mutex::new(0),
        chatty: false,
    };
    let (mut book, dir) = load_book(&dir).unwrap();
    let report = refine(&mut book, &dir, &model, &refine_opts(), &|_| {}).unwrap();
    save_book(&book, &dir).unwrap();
    assert_eq!(report.chapters_written, 2);
    assert_eq!(report.sent, 2);
    assert!(report.fallbacks.is_empty() && report.failed.is_empty());

    let built = build_book(&dir, &BuildOptions::default()).unwrap();
    assert_eq!(built.text_stage.as_deref(), Some("refined"));
    let spoken: String = built.chapters[0]
        .segments
        .iter()
        .map(|s| s.text.clone())
        .collect();
    assert!(spoken.contains("Table 1-1: Supported Systems, lists one system, Linux"));
    assert!(spoken.contains("Figure 1-1: The setup screen shows the first screen."));
    assert!(!spoken.contains("<!--"));
    // The heading travels with its paragraph; the title stays alone.
    assert_eq!(built.chapters[0].segments[0].text, "Chapter 1: Setup.");

    // Hand edits survive a re-run, and are what `build` speaks.
    let text_file = dir.join(built.chapters[1].refined_file.as_ref().unwrap());
    std::fs::write(&text_file, "# Chapter 2: Usage\n\nEdited by hand.\n").unwrap();
    let (mut book, _) = load_book(&dir).unwrap();
    let report = refine(&mut book, &dir, &model, &refine_opts(), &|_| {}).unwrap();
    assert_eq!(report.chapters_kept, 2);
    assert_eq!(*model.calls.lock().unwrap(), 2);
    let built = build_book(&dir, &BuildOptions::default()).unwrap();
    assert_eq!(built.chapters[1].segments[1].text, "Edited by hand.");

    // --force re-refines from the cache without calling the model.
    let (mut book, _) = load_book(&dir).unwrap();
    let opts = RefineOptions {
        force: true,
        ..refine_opts()
    };
    let report = refine(&mut book, &dir, &model, &opts, &|_| {}).unwrap();
    assert_eq!((report.cached, report.sent), (2, 0));
    assert_eq!(*model.calls.lock().unwrap(), 2);
    assert!(
        !std::fs::read_to_string(&text_file)
            .unwrap()
            .contains("Edited by hand")
    );
}

#[test]
fn rejected_answers_fall_back_to_extracted_text() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = extracted_tech_book(tmp.path());
    let model = FakeModel {
        calls: Mutex::new(0),
        chatty: true,
    };
    let (mut book, dir) = load_book(&dir).unwrap();
    let opts = RefineOptions {
        chapters: Some(vec![1]),
        ..refine_opts()
    };
    let report = refine(&mut book, &dir, &model, &opts, &|_| {}).unwrap();
    // One retry with the reason, then the extracted text is kept.
    assert_eq!(*model.calls.lock().unwrap(), 2);
    assert_eq!(report.fallbacks.len(), 1);
    assert!(report.fallbacks[0].2.contains("commentary"));
    assert_eq!(report.chapters_written, 1);
    save_book(&book, &dir).unwrap();
    let built = build_book(&dir, &BuildOptions::default()).unwrap();
    assert_eq!(built.text_stage.as_deref(), Some("partial"));
}

/// Abridges like a well-behaved model: keeps the title, the first paragraph
/// of each section and drops the rest.
struct FakeAbridger;

impl ChatBackend for FakeAbridger {
    fn chat(&self, _model: &str, messages: &[ChatMessage]) -> Result<String, ChatError> {
        assert!(messages[0].content.contains("about 40% of"));
        let input = messages[1]
            .content
            .rsplit("TEXT TO ABRIDGE:\n<<<\n")
            .next()
            .unwrap()
            .trim_end_matches(">>>")
            .trim();
        let mut out = Vec::new();
        let mut kept_para = false;
        for block in input.split("\n\n") {
            if block.starts_with('#') {
                out.push(block.to_string());
                kept_para = false;
            } else if !kept_para {
                out.push(block.to_string());
                kept_para = true;
            }
        }
        Ok(out.join("\n\n"))
    }
}

#[test]
fn abridge_makes_a_separate_shorter_book() {
    use tts_audiobook_generator::abridge;

    let tmp = tempfile::tempdir().unwrap();
    let long_chapter = (1..=6)
        .map(|i| {
            format!(
                "Paragraph {i} of the chapter. {}",
                "Some words here. ".repeat(20)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let md = tmp.path().join("long.md");
    std::fs::write(
        &md,
        format!(
            "---\ntitle: Long Notes\n---\n\n# Chapter 1: One\n\n{long_chapter}\n\n# Chapter 2: Two\n\n{long_chapter}\n\n# Appendix A: Extra\n\n{long_chapter}\n"
        ),
    )
    .unwrap();
    let mut book = extract_book(&md, &ExtractOptions::default()).unwrap();
    let src = write_book(&mut book, &tmp.path().join("out"), true).unwrap();

    let dst = abridge::default_dir(&src);
    assert!(dst.ends_with("long-notes-abridged"));
    let prepared = abridge::prepare(&src, &dst, &[3]).unwrap();
    assert_eq!(
        prepared.metadata.title.as_deref(),
        Some("Long Notes (Abridged)")
    );
    assert_eq!(prepared.chapters.len(), 2);

    let (mut abridged, dst) = load_book(&dst).unwrap();
    let opts = RefineOptions {
        task: Task::Abridge { percent: 40 },
        ..refine_opts()
    };
    let report = refine(&mut abridged, &dst, &FakeAbridger, &opts, &|_| {}).unwrap();
    assert_eq!(report.chapters_written, 2);
    assert!(report.fallbacks.is_empty(), "{:?}", report.fallbacks);
    save_book(&abridged, &dst).unwrap();
    let built = build_book(&dst, &BuildOptions::default()).unwrap();
    assert_eq!(built.text_stage.as_deref(), Some("refined"));

    let words = |b: &Book| b.chapters.iter().map(|c| c.word_count).sum::<usize>();
    let full = load_book(&src).unwrap().0;
    assert!(
        words(&built) * 2 < words(&full),
        "abridged book must be much shorter"
    );
    assert_eq!(built.chapters[0].segments[0].text, "Chapter 1: One.");
    // The full book is untouched.
    assert_eq!(full.text_stage.as_deref(), Some("raw"));
    assert!(!src.join("text").exists());
    // An abridged book cannot be abridged again.
    assert!(abridge::prepare(&dst, &tmp.path().join("again"), &[]).is_err());
}
