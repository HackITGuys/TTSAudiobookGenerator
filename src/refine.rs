//! Optional stage between `extract` and `build`: an LLM repairs the
//! extracted Markdown (`raw/`) and rewrites what cannot be read aloud as is
//! (tables, figures, code, hashes, URLs), writing `text/`.
//!
//! - Chapters are cut into chunks at paragraph boundaries; a table is never
//!   split. The end of the previous chunk is sent along as read-only context.
//! - Every answer is checked (headings kept, no commentary, plausible
//!   length). A rejected answer is retried once with the reason; if it fails
//!   again the chunk keeps its extracted text and is reported.
//! - Accepted answers are cached in `refine/cache/<sha256>.md`, keyed by
//!   prompt version, model, context and input, so re-runs and interrupted
//!   runs only pay for chunks that changed.
//! - An existing `text/` file is kept unless `force` is set, so hand edits
//!   survive re-runs.

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::llm::{ChatBackend, ChatError, ChatMessage};
use crate::model::Book;
use crate::output::{TEXT_DIR, chapter_stem};

/// Bump when the prompt changes so cached answers are not reused.
pub const PROMPT_VERSION: u32 = 1;
/// Characters of raw Markdown per LLM request.
pub const DEFAULT_CHUNK_CHARS: usize = 6000;
/// Characters of the previous chunk sent as context.
const CONTEXT_CHARS: usize = 800;
pub const CACHE_DIR: &str = "refine/cache";
/// Rejected answers, kept for inspection.
pub const REJECTED_DIR: &str = "refine/rejected";

pub const SYSTEM_PROMPT: &str = r##"You are a careful copy editor preparing the text of a book for an audiobook. The text was extracted automatically from a PDF or e-book and will be read aloud by a text-to-speech voice. A listener cannot see tables, figures, code or links, so those must be put into words.

The input is Markdown: "#" lines are headings, blank lines separate paragraphs, "- " and "1. " lines are list items, "* * *" is a section break. A table is marked by a line "<!-- table: CAPTION -->" followed by its rows as "| cell | cell |" lines. A figure is marked by a line "<!-- figure: CAPTION -->".

Rules:
1. Repair extraction damage only: words split or joined by mistake ("identifi cation" becomes "identification"), hyphens left over from line breaks ("authen- tication" becomes "authentication"), leftover page numbers, running headers and footers, index-style lists of page numbers, labels and fragments that came from inside a diagram, stray symbols and bullets, and paragraphs that were split in the middle of a sentence. Do not summarize, shorten, simplify, modernize or reword the author's prose: leave correct sentences exactly as they are.
2. Tables: replace the marker and its rows with prose that begins "Table X-Y, <caption>, ...". For a small table (about six rows or fewer) give one sentence per row. For a larger table, say what the columns are, then summarize the rows in a few sentences, naming the most important entries. Only report what the table contains.
3. Figures: replace the marker with one or two sentences such as "Figure X-Y shows ...", based on the caption and the surrounding text. Do not invent details the text does not give. Remove diagram labels near it that are not part of the prose.
4. Code, commands, configuration, log output and stack traces:
   - 5 lines or fewer: keep them, written so they can be spoken. Say operators and symbols in words ("x equals y plus one", "slash etc slash passwd"), drop punctuation that means nothing when heard (braces, semicolons), and apply rules 5 and 6 inside them.
   - More than 5 lines: do not read them out. Explain in one or two sentences what the code does, for example "The listing shows a C function that copies user input into a fixed-size buffer without checking its length." Keep a short identifier only if the prose refers to it.
5. Hashes, keys, tokens, serial numbers and IDs: name only their type, for example "a SHA-256 hash", "an MD5 hash", "a UUID", "a base64-encoded value". Never spell them out.
6. URLs: say only the host name ("docs.example.com"), without the scheme, "www.", path or query. An e-mail address becomes "an e-mail address at example.com".
7. Write symbols and abbreviations the way a narrator would say them where reading them literally would sound wrong: "->" becomes "to", "e.g." becomes "for example", "i.e." becomes "that is", "§ 3" becomes "section 3", "&" becomes "and".
8. Keep every heading line, in the same order, as a heading; only repair extraction damage in it. Keep the Markdown structure of headings, paragraphs, list items and section breaks.
9. Answer with the repaired Markdown only: no preamble, no notes, no code fences, and never repeat the PREVIOUS TEXT."##;

/// Instructions for `abridge`; `{PERCENT}` is replaced by the target length.
pub const ABRIDGE_PROMPT: &str = r##"You are abridging a book for an abridged audiobook edition. Cut the text you are given to about {PERCENT}% of its length by removing what a listener can do without, while keeping the author's voice, the line of argument or story, and everything needed to follow later chapters.

The input is Markdown that is already prepared for speech: "#" lines are headings, blank lines separate paragraphs, "- " and "1. " lines are list items, "* * *" is a section break.

Remove first:
- repetition: restatements, recaps, previews ("in this chapter we will..."), and points made twice;
- extra examples, when one example already makes the point; long anecdotes and asides that are not essential;
- cross-references to other parts of the book, pages, figures or tables ("as discussed in Chapter 3", "see Table 4-2");
- long lists: keep the most important items and say that there are more if that matters;
- detailed reference material: long descriptions of tables, figures, data or code become one sentence, or go if they add nothing;
- long quotations, acknowledgements and similar material.

Keep: key definitions and named concepts (especially ones used later), the main arguments and conclusions, essential steps, and the headings of the sections you keep.

Rules:
1. Cut, do not rewrite. Keep the author's own sentences and words. You may drop sentences, paragraphs and whole sections, shorten a sentence, or add a few connecting words where a cut leaves a gap. Never add facts, opinions or summaries of your own.
2. Keep the first "# " heading line if there is one. You may drop other heading lines together with their whole section, but never add, rename or reorder headings.
3. Keep the Markdown structure and keep the text speakable (no URLs, code, symbols or markup).
4. Answer with the abridged Markdown only: no preamble, no notes, no code fences, and never repeat the PREVIOUS TEXT."##;

/// What the LLM does to each chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    /// Repair extraction damage and make the text speakable (`refine`).
    Repair,
    /// Cut the text to about `percent` % of its length (`abridge`).
    Abridge { percent: u8 },
}

impl Task {
    pub fn system_prompt(&self) -> String {
        match self {
            Task::Repair => SYSTEM_PROMPT.to_string(),
            Task::Abridge { percent } => ABRIDGE_PROMPT.replace("{PERCENT}", &percent.to_string()),
        }
    }

    /// Distinguishes cache entries of different tasks (empty for `Repair`,
    /// so existing caches stay valid).
    fn key_prefix(&self) -> String {
        match self {
            Task::Repair => String::new(),
            Task::Abridge { percent } => format!("abridge{percent}\0"),
        }
    }

    /// Whether a minor issue is worth a second request. For `Abridge` it is
    /// not: a model that cut to 45% instead of 25% does the same again, so a
    /// retry only doubles the cost.
    fn retry_on_notes(&self) -> bool {
        matches!(self, Task::Repair)
    }

    fn label(&self) -> &'static str {
        match self {
            Task::Repair => "TEXT TO REPAIR",
            Task::Abridge { .. } => "TEXT TO ABRIDGE",
        }
    }

    fn validate(&self, input: &str, output: &str) -> std::result::Result<Vec<String>, String> {
        match self {
            Task::Repair => validate(input, output),
            Task::Abridge { percent } => validate_abridged(input, output, *percent),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RefineOptions {
    pub task: Task,
    pub model: String,
    pub concurrency: usize,
    /// Re-refine chapters that already have a `text/` file.
    pub force: bool,
    /// 1-based chapter numbers; `None` = all.
    pub chapters: Option<Vec<usize>>,
    pub chunk_chars: usize,
}

#[derive(Debug, Default, PartialEq)]
pub struct RefineReport {
    /// Chapters whose `text/` file was written in this run.
    pub chapters_written: usize,
    /// Chapters that already had a `text/` file and were left alone.
    pub chapters_kept: usize,
    pub chunks: usize,
    /// Chunks answered from the cache.
    pub cached: usize,
    /// Chunks sent to the model.
    pub sent: usize,
    pub chars_sent: usize,
    /// (chapter, chunk, reason): answers rejected twice; the extracted text
    /// was kept for these chunks.
    pub fallbacks: Vec<(usize, usize, String)>,
    /// (chapter, chunk, note): answers accepted with a minor issue, e.g. a
    /// table left as a marker (spoken as a reference by `build`).
    pub notes: Vec<(usize, usize, String)>,
    /// (chapter, chunk, error): requests that failed; the chapter was not
    /// written and is retried on the next run.
    pub failed: Vec<(usize, usize, String)>,
}

/// What a dry run reports.
#[derive(Debug, Default, PartialEq)]
pub struct RefinePlan {
    pub chapters: usize,
    pub chapters_kept: usize,
    pub chunks: usize,
    pub cached: usize,
    /// Characters (input plus context) still to be sent.
    pub chars: usize,
}

struct Job {
    /// Position in `book.chapters`.
    pos: usize,
    chunk: usize,
    chunks: usize,
    input: String,
    context: String,
    key: String,
}

struct ChapterWork {
    pos: usize,
    text_rel: String,
    jobs: std::ops::Range<usize>,
}

enum Outcome {
    Cached,
    Sent,
    /// Accepted on the retry despite a minor issue.
    Accepted(String),
    Fallback(String),
}

/// A chunk's refined text and how it was obtained, or the error.
type JobResult = std::result::Result<(String, Outcome), String>;
/// Chapters to refine, their chunk jobs, and chapters kept as they are.
type Collected = (Vec<ChapterWork>, Vec<Job>, Vec<(usize, String)>);

/// Cut a chapter's Markdown into chunks of at most `max` characters at
/// blank-line boundaries. A block (paragraph, or a table with its rows) is
/// never split; one longer than `max` becomes a chunk of its own.
pub fn split_chunks(md: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for block in md.split("\n\n").map(str::trim).filter(|b| !b.is_empty()) {
        let len = block.chars().count();
        if !cur.is_empty() && cur.chars().count() + 2 + len > max {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(block);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// The last block of a chunk, cut to `CONTEXT_CHARS` from its end.
fn context_of(chunk: &str) -> String {
    let last = chunk.rsplit("\n\n").next().unwrap_or("").trim();
    let n = last.chars().count();
    if n <= CONTEXT_CHARS {
        last.to_string()
    } else {
        last.chars().skip(n - CONTEXT_CHARS).collect()
    }
}

fn cache_key(task: Task, model: &str, context: &str, input: &str) -> String {
    let mut h = Sha256::new();
    h.update(
        format!(
            "{}v{PROMPT_VERSION}\0{model}\0{context}\0{input}",
            task.key_prefix()
        )
        .as_bytes(),
    );
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn user_message(task: Task, context: &str, input: &str) -> String {
    let mut m = String::new();
    if !context.is_empty() {
        m.push_str("PREVIOUS TEXT (context only; do not include it in your answer):\n<<<\n");
        m.push_str(context);
        m.push_str("\n>>>\n\n");
    }
    m.push_str(task.label());
    m.push_str(":\n<<<\n");
    m.push_str(input);
    m.push_str("\n>>>");
    m
}

/// Remove wrapping the model may add despite the instructions: code fences
/// and the `<<<`/`>>>` delimiters.
fn unwrap_answer(answer: &str) -> String {
    let mut s = answer.trim();
    if s.starts_with("```") {
        s = s.split_once('\n').map_or("", |(_, rest)| rest);
        s = s.trim_end().strip_suffix("```").unwrap_or(s);
    }
    let s = s.trim();
    let s = s.strip_prefix("<<<").unwrap_or(s);
    let s = s.strip_suffix(">>>").unwrap_or(s);
    s.trim().to_string()
}

/// Lower-case letters and digits only, for comparing headings.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn headings(md: &str) -> Vec<String> {
    md.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('#') && l.trim_start_matches('#').starts_with(' '))
        .map(|l| norm(l.trim_start_matches('#')))
        .collect()
}

/// Characters of the input that are prose (not table or figure blocks).
fn prose_chars(md: &str) -> usize {
    md.split("\n\n")
        .map(str::trim)
        .filter(|b| !b.starts_with("<!--") && !b.starts_with('|'))
        .map(|b| b.chars().count())
        .sum()
}

/// Check an answer against its input; `Err` names what is wrong.
///
/// `Ok` carries minor issues (notes) that are worth one retry but do not
/// make the answer unusable: a table or figure marker left in is spoken as
/// a reference by `build`.
pub fn validate(input: &str, output: &str) -> std::result::Result<Vec<String>, String> {
    if output.trim().is_empty() {
        return Err("the answer is empty".into());
    }
    let first = output.trim_start().to_lowercase();
    if [
        "here is",
        "here's",
        "sure",
        "certainly",
        "below is",
        "i have",
        "i've",
    ]
    .iter()
    .any(|p| first.starts_with(p))
    {
        return Err("the answer starts with commentary instead of the text".into());
    }
    let mut notes = Vec::new();
    if output.contains("<!-- table") || output.contains("<!-- figure") {
        notes.push("a table or figure marker was left in; replace it with spoken prose".into());
    }
    // Every heading of the input is kept, in order. Extra headings are fine
    // when they are lines of the input promoted to headings (PDFs often set
    // sub-headings in body type), but not when they are invented.
    let (want, got) = (headings(input), headings(output));
    let mut remaining = got.iter();
    if !want.iter().all(|w| remaining.any(|g| g == w)) {
        return Err(format!(
            "a heading line was dropped or changed (expected these {} in order); keep every heading line",
            want.len()
        ));
    }
    let source = norm(input);
    if got.iter().any(|g| !source.contains(g.as_str())) {
        return Err(
            "a heading was added that is not in the text; only existing lines may become headings"
                .into(),
        );
    }
    let prose = prose_chars(input);
    let total = input.chars().count();
    let out = output.chars().count();
    if prose >= 400 && out * 2 < prose {
        return Err(format!(
            "the answer is much shorter than the input ({out} of {prose} characters); do not summarize the prose"
        ));
    }
    if out > total * 8 / 5 + 600 {
        return Err(format!(
            "the answer is much longer than the input ({out} vs {total} characters)"
        ));
    }
    Ok(notes)
}

/// Check an abridged answer: no commentary, headings only dropped (never
/// added, renamed or reordered), the chapter title kept, and a length near
/// `percent` % of the input.
pub fn validate_abridged(
    input: &str,
    output: &str,
    percent: u8,
) -> std::result::Result<Vec<String>, String> {
    if output.trim().is_empty() {
        return Err("the answer is empty".into());
    }
    let first = output.trim_start().to_lowercase();
    if [
        "here is",
        "here's",
        "sure",
        "certainly",
        "below is",
        "i have",
        "i've",
    ]
    .iter()
    .any(|p| first.starts_with(p))
    {
        return Err("the answer starts with commentary instead of the text".into());
    }
    // Headings of the input may only be dropped, not reordered. Other
    // headings are fine when they are lines of the input promoted to
    // headings (sub-headings set in body type), but not when invented.
    let (have, got) = (headings(input), headings(output));
    let mut remaining = have.iter();
    if !got
        .iter()
        .filter(|g| have.contains(g))
        .all(|g| remaining.any(|h| h == g))
    {
        return Err(
            "headings were reordered; you may only drop headings with their section".into(),
        );
    }
    let source = norm(input);
    if got.iter().any(|g| !source.contains(g.as_str())) {
        return Err(
            "a heading was added that is not in the text; you may only drop headings with their section"
                .into(),
        );
    }
    let title = input
        .lines()
        .find(|l| l.starts_with("# "))
        .map(|l| norm(l.trim_start_matches('#')));
    if let Some(title) = title
        && !got.contains(&title)
    {
        return Err("the chapter title (the first \"# \" line) was dropped; keep it".into());
    }
    let mut notes = Vec::new();
    if output.contains("<!-- table") || output.contains("<!-- figure") {
        notes.push("a table or figure marker was left in".into());
    }
    let total = input.chars().count();
    let out = output.chars().count();
    let target = total * usize::from(percent) / 100;
    if total >= 1500 {
        // Must really cut, but not hollow the text out. Missing the target
        // is only a note: a chunk cut to 45% instead of 25% is still far
        // better than falling back to the full text.
        if out > total * 19 / 20 {
            return Err(format!(
                "the text was hardly cut ({out} of {total} characters); cut it to about {percent}% ({target} characters)"
            ));
        }
        if out > target * 8 / 5 {
            notes.push(format!(
                "the answer is too long ({out} of {total} characters); cut it to about {percent}% ({target} characters)"
            ));
        }
        if out * 5 < target * 2 {
            return Err(format!(
                "too much was cut ({out} of {total} characters); aim for about {percent}% ({target} characters)"
            ));
        }
    } else if out > total + total / 20 {
        return Err("the answer is longer than the input".into());
    }
    Ok(notes)
}

fn selected(book: &Book, opts: &RefineOptions) -> Vec<usize> {
    (0..book.chapters.len())
        .filter(|&i| {
            opts.chapters
                .as_ref()
                .is_none_or(|list| list.contains(&book.chapters[i].index))
        })
        .collect()
}

/// Chapters to refine and their chunk jobs; also returns chapters that are
/// kept because their `text/` file already exists.
fn collect(book: &Book, dir: &Path, opts: &RefineOptions) -> Result<Collected> {
    let mut work = Vec::new();
    let mut jobs = Vec::new();
    let mut kept = Vec::new();
    for pos in selected(book, opts) {
        let ch = &book.chapters[pos];
        let text_rel = format!("{TEXT_DIR}/{}.md", chapter_stem(ch));
        if dir.join(&text_rel).is_file() && !opts.force {
            kept.push((pos, text_rel));
            continue;
        }
        let Some(raw_rel) = &ch.raw_file else {
            bail!(
                "chapter {} has no raw/ Markdown; re-run `extract` with this version",
                ch.index
            );
        };
        let md = fs::read_to_string(dir.join(raw_rel))
            .with_context(|| format!("cannot read {}", dir.join(raw_rel).display()))?;
        let chunks = split_chunks(&md, opts.chunk_chars.max(500));
        let start = jobs.len();
        let n = chunks.len();
        let mut context = String::new();
        for (i, input) in chunks.into_iter().enumerate() {
            let next_context = context_of(&input);
            jobs.push(Job {
                pos,
                chunk: i + 1,
                chunks: n,
                key: cache_key(opts.task, &opts.model, &context, &input),
                input,
                context: std::mem::replace(&mut context, next_context),
            });
        }
        work.push(ChapterWork {
            pos,
            text_rel,
            jobs: start..jobs.len(),
        });
    }
    Ok((work, jobs, kept))
}

fn cache_path(dir: &Path, key: &str) -> std::path::PathBuf {
    dir.join(CACHE_DIR).join(format!("{key}.md"))
}

/// What `refine` would do, without calling the model.
pub fn plan(book: &Book, dir: &Path, opts: &RefineOptions) -> Result<RefinePlan> {
    let (work, jobs, kept) = collect(book, dir, opts)?;
    let mut plan = RefinePlan {
        chapters: work.len(),
        chapters_kept: kept.len(),
        chunks: jobs.len(),
        ..Default::default()
    };
    for job in &jobs {
        if cache_path(dir, &job.key).is_file() {
            plan.cached += 1;
        } else {
            plan.chars += job.input.chars().count() + job.context.chars().count();
        }
    }
    Ok(plan)
}

fn run_job(
    backend: &dyn ChatBackend,
    task: Task,
    model: &str,
    dir: &Path,
    job: &Job,
) -> std::result::Result<(String, Outcome), ChatError> {
    let cached = cache_path(dir, &job.key);
    if let Ok(text) = fs::read_to_string(&cached) {
        return Ok((text, Outcome::Cached));
    }
    let mut messages = vec![
        ChatMessage::system(task.system_prompt()),
        ChatMessage::user(user_message(task, &job.context, &job.input)),
    ];
    for attempt in 0..2 {
        let answer = backend.chat(model, &messages)?;
        let text = unwrap_answer(&answer);
        let verdict = task.validate(&job.input, &text);
        let reason = match &verdict {
            Ok(notes) if notes.is_empty() || attempt == 1 || !task.retry_on_notes() => {
                let tmp = cached.with_extension("md.part");
                if fs::write(&tmp, &text).is_ok() {
                    let _ = fs::rename(&tmp, &cached);
                }
                let outcome = match notes.is_empty() {
                    true => Outcome::Sent,
                    false => Outcome::Accepted(notes.join("; ")),
                };
                return Ok((text, outcome));
            }
            Ok(notes) => notes.join("; "),
            Err(reason) => reason.clone(),
        };
        // Keep rejected answers for inspection.
        let rejected = dir.join(REJECTED_DIR);
        if fs::create_dir_all(&rejected).is_ok() {
            let _ = fs::write(
                rejected.join(format!("{}-{}.md", job.key, attempt + 1)),
                format!("<!-- rejected: {reason} -->\n\n{answer}\n"),
            );
        }
        if attempt == 1 {
            return Ok((job.input.clone(), Outcome::Fallback(reason)));
        }
        messages.push(ChatMessage::assistant(answer));
        messages.push(ChatMessage::user(format!(
            "That answer was rejected: {reason}. Do the same {} again, following every rule, and answer with the Markdown only.",
            task.label()
        )));
    }
    unreachable!("the loop returns on its second attempt")
}

/// Refine the selected chapters and record their `text/` files in `book`.
/// The caller saves `book.json` and runs `build`.
pub fn refine(
    book: &mut Book,
    dir: &Path,
    backend: &dyn ChatBackend,
    opts: &RefineOptions,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<RefineReport> {
    let (work, jobs, kept) = collect(book, dir, opts)?;
    fs::create_dir_all(dir.join(TEXT_DIR))?;
    fs::create_dir_all(dir.join(CACHE_DIR))?;

    let mut report = RefineReport {
        chapters_kept: kept.len(),
        chunks: jobs.len(),
        ..Default::default()
    };
    for (pos, rel) in kept {
        book.chapters[pos].refined_file = Some(rel);
    }

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let fatal: Mutex<Option<String>> = Mutex::new(None);
    let results: Mutex<Vec<Option<JobResult>>> =
        Mutex::new((0..jobs.len()).map(|_| None).collect());
    let titles: Vec<String> = book.chapters.iter().map(|c| c.title.clone()).collect();
    let indices: Vec<usize> = book.chapters.iter().map(|c| c.index).collect();

    thread::scope(|scope| {
        for _ in 0..opts.concurrency.clamp(1, 16) {
            scope.spawn(|| {
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let outcome = match run_job(backend, opts.task, &opts.model, dir, job) {
                        Ok(r) => Ok(r),
                        Err(ChatError::Fatal(m)) => {
                            abort.store(true, Ordering::Relaxed);
                            fatal.lock().unwrap().get_or_insert(m.clone());
                            Err(m)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let status = match &outcome {
                        Ok((_, Outcome::Cached)) => "cached".to_string(),
                        Ok((_, Outcome::Sent)) => "ok".to_string(),
                        Ok((_, Outcome::Accepted(r))) => format!("ok, with a note ({r})"),
                        Ok((_, Outcome::Fallback(r))) => format!("kept extracted text ({r})"),
                        Err(e) => format!("FAILED: {e}"),
                    };
                    progress(&format!(
                        "[{n}/{}] chapter {} ({}) chunk {}/{} {status}",
                        jobs.len(),
                        indices[job.pos],
                        titles[job.pos],
                        job.chunk,
                        job.chunks
                    ));
                    results.lock().unwrap()[i] = Some(outcome);
                }
            });
        }
    });

    let results = results.into_inner().unwrap();
    for w in work {
        let mut parts = Vec::new();
        let mut complete = true;
        for i in w.jobs {
            let job = &jobs[i];
            let chapter = indices[job.pos];
            match &results[i] {
                Some(Ok((text, outcome))) => {
                    match outcome {
                        Outcome::Cached => report.cached += 1,
                        Outcome::Sent => {
                            report.sent += 1;
                            report.chars_sent +=
                                job.input.chars().count() + job.context.chars().count();
                        }
                        Outcome::Accepted(r) | Outcome::Fallback(r) => {
                            report.sent += 1;
                            report.chars_sent +=
                                job.input.chars().count() + job.context.chars().count();
                            let list = match outcome {
                                Outcome::Accepted(_) => &mut report.notes,
                                _ => &mut report.fallbacks,
                            };
                            list.push((chapter, job.chunk, r.clone()));
                        }
                    }
                    parts.push(text.trim().to_string());
                }
                Some(Err(e)) => {
                    report.failed.push((chapter, job.chunk, e.clone()));
                    complete = false;
                }
                None => complete = false, // not run: aborted
            }
        }
        if complete {
            let path = dir.join(&w.text_rel);
            fs::write(&path, parts.join("\n\n") + "\n")
                .with_context(|| format!("cannot write {}", path.display()))?;
            book.chapters[w.pos].refined_file = Some(w.text_rel);
            report.chapters_written += 1;
        }
    }

    if let Some(m) = fatal.into_inner().unwrap() {
        bail!("{m}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_chunks_at_blank_lines_without_splitting_tables() {
        let table = "<!-- table: Table 1-1: Things -->\n| a | b |\n| c | d |";
        let md = format!(
            "# Title\n\n{}\n\n{table}\n\n{}\n",
            "x".repeat(40),
            "y".repeat(40)
        );
        let chunks = split_chunks(&md, 60);
        assert_eq!(
            chunks,
            vec![
                format!("# Title\n\n{}", "x".repeat(40)),
                table.to_string(),
                "y".repeat(40),
            ]
        );
        assert_eq!(context_of(&chunks[0]), "x".repeat(40));
    }

    #[test]
    fn unwraps_fenced_answers() {
        assert_eq!(
            unwrap_answer("```markdown\n# A\n\nText.\n```"),
            "# A\n\nText."
        );
        assert_eq!(unwrap_answer("<<<\nText.\n>>>"), "Text.");
    }

    #[test]
    fn validates_answers() {
        let input = format!(
            "# Chapter 1: Dive In\n\n{}\n\n## Spoofi ng\n\nMore.",
            "word ".repeat(100)
        );
        let good = format!(
            "# Chapter 1: Dive In\n\n{}\n\n## Spoofing\n\nMore.",
            "word ".repeat(100)
        );
        assert_eq!(validate(&input, &good), Ok(vec![]));
        assert!(validate(&input, &format!("Here is the text:\n{good}")).is_err());
        assert!(
            validate(
                &input,
                "# Chapter 1: Dive In\n\nShort.\n\n## Spoofing\n\nMore."
            )
            .is_err()
        );
        // A dropped heading is rejected.
        assert!(validate(&input, &good.replace("## Spoofing\n\n", "")).is_err());
        // A line of the input promoted to a heading is fine; an invented one is not.
        let promoted = good.replace("\n\nMore.", "\n\n### More.");
        assert_eq!(validate(&input, &promoted), Ok(vec![]));
        let invented = good.replace("\n\nMore.", "\n\n### A New Section\n\nMore.");
        assert!(validate(&input, &invented).is_err());
        // A table left as a marker is only a note (worth a retry).
        let notes = validate(
            "<!-- table: Table 1-1: T -->\n| a |",
            "<!-- table: Table 1-1: T -->\nTable 1-1 ...",
        )
        .unwrap();
        assert_eq!(notes.len(), 1);
        // Tables may be explained at length; short prose may change length.
        assert_eq!(
            validate(
                "<!-- table: Table 1-1: T -->\n| a | b |",
                &"Table 1-1, T, lists a. ".repeat(20)
            ),
            Ok(vec![])
        );
    }

    #[test]
    fn cache_key_depends_on_task_model_context_and_input() {
        let r = Task::Repair;
        let k = cache_key(r, "m", "c", "i");
        // The repair key format must not change, or existing caches are lost.
        let expected: String = Sha256::digest(format!("v{PROMPT_VERSION}\0m\0c\0i").as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(k, expected);
        assert_ne!(k, cache_key(r, "m2", "c", "i"));
        assert_ne!(k, cache_key(r, "m", "c2", "i"));
        assert_ne!(k, cache_key(r, "m", "c", "i2"));
        let a50 = cache_key(Task::Abridge { percent: 50 }, "m", "c", "i");
        assert_ne!(k, a50);
        assert_ne!(a50, cache_key(Task::Abridge { percent: 40 }, "m", "c", "i"));
        assert!(
            Task::Abridge { percent: 40 }
                .system_prompt()
                .contains("about 40% of")
        );
    }

    #[test]
    fn validates_abridged_answers() {
        let para = |w: &str| format!("{} end.", w.repeat(120));
        let input = format!(
            "# Chapter 2: Usage\n\n{}\n\n## Details\n\n{}\n\n## Extras\n\n{}",
            para("alpha "),
            para("beta "),
            para("gamma ")
        );
        // Dropping a section with its heading, to about half: fine.
        let half = format!(
            "# Chapter 2: Usage\n\n{}\n\n## Extras\n\n{}",
            para("alpha "),
            "gamma ".repeat(40)
        );
        assert_eq!(validate_abridged(&input, &half, 50), Ok(vec![]));
        // Not cut at all.
        assert!(validate_abridged(&input, &input, 50).is_err());
        // Hollowed out.
        assert!(validate_abridged(&input, "# Chapter 2: Usage\n\nShort.", 50).is_err());
        // Title dropped, heading invented, headings reordered.
        assert!(
            validate_abridged(&input, &half.replace("# Chapter 2: Usage\n\n", ""), 50).is_err()
        );
        assert!(validate_abridged(&input, &half.replace("## Extras", "## Summary"), 50).is_err());
        let reordered = format!(
            "# Chapter 2: Usage\n\n## Extras\n\n{}\n\n## Details\n\n{}",
            para("gamma "),
            "beta ".repeat(30)
        );
        assert!(validate_abridged(&input, &reordered, 50).is_err());
        // A line of the input promoted to a heading is fine.
        let promoted = half.replace("## Extras", "## Extras\n\n### gamma gamma end");
        assert_eq!(validate_abridged(&input, &promoted, 50), Ok(vec![]));
        // Cut, but not as far as asked: kept, with a note.
        let notes = validate_abridged(&input, &half, 25).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("too long"));
        // Short chunks may be kept as they are.
        assert_eq!(
            validate_abridged("Short text.", "Short text.", 50),
            Ok(vec![])
        );
    }
}
