//! Stage 2: turn the segments in `book.json` into audio files.
//!
//! One file per segment is written to
//! `<book>/audio/NNN-<chapter>/SSSS.<format>`, plus the spoken opening credits
//! and closing AI disclaimer in `audio/000-opening-credits/` and
//! `audio/999-closing-credits/`. Next to each audio file, `SSSS.txt` holds
//! the exact text it was made from. Runs are resumable: a segment is skipped
//! only if its audio exists and that text still matches, so re-extracting a
//! book (which renumbers segments) never reuses audio for the wrong text.
//! Files are written to a `.part` file first so an interrupted run never
//! leaves a truncated file behind.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};

use crate::credits;
use crate::mistral::{SpeechBackend, SpeechError, SpeechRequest};
use crate::model::{Book, Credits, Segment, TtsInfo};
use crate::output::slugify;
use crate::sentences::split_sentences;

/// Segment formats the assembly stage can read.
pub const FORMATS: &[&str] = &["flac", "wav", "mp3", "opus"];
pub const OPENING_STEM: &str = "000-opening-credits";
pub const CLOSING_STEM: &str = "999-closing-credits";

#[derive(Debug, Clone)]
pub struct SynthOptions {
    pub model: String,
    pub voice: Option<String>,
    pub format: String,
    pub concurrency: usize,
    /// Regenerate segments that already have audio.
    pub force: bool,
    /// 1-based chapter numbers to synthesise; `None` = all.
    pub chapters: Option<Vec<usize>>,
}

impl SynthOptions {
    fn tts_info(&self) -> TtsInfo {
        TtsInfo {
            provider: "mistral".into(),
            model: self.model.clone(),
            voice: self.voice.clone(),
            format: self.format.clone(),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct SynthReport {
    pub synthesized: usize,
    pub skipped: usize,
    /// (part label, segment number, error)
    pub failed: Vec<(String, usize, String)>,
    /// Characters sent to the API.
    pub characters: usize,
    /// Segments that moderation blocked as a whole but that succeeded when
    /// synthesised in smaller pieces.
    pub split_by_moderation: usize,
}

/// Accept either a book directory or the path of its `book.json`.
pub fn book_json_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("book.json")
    } else {
        path.to_path_buf()
    }
}

pub fn load_book(path: &Path) -> Result<(Book, PathBuf)> {
    let json_path = book_json_path(path);
    let text = fs::read_to_string(&json_path)
        .with_context(|| format!("cannot read {} (run `extract` first)", json_path.display()))?;
    let book: Book =
        serde_json::from_str(&text).with_context(|| format!("invalid {}", json_path.display()))?;
    let dir = json_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Ok((book, dir))
}

pub fn save_book(book: &Book, dir: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(book)?;
    fs::write(dir.join("book.json"), json + "\n")?;
    Ok(())
}

/// Parse a chapter selection such as `1,3-5`.
pub fn parse_chapter_list(s: &str) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse()?, b.trim().parse()?);
                if a > b {
                    bail!("invalid chapter range '{part}'");
                }
                out.extend(a..=b);
            }
            None => out.push(part.parse()?),
        }
    }
    Ok(out)
}

/// A run of segments rendered into one audio directory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Part {
    OpeningCredits,
    /// Position in `book.chapters`.
    Chapter(usize),
    ClosingCredits,
}

impl Part {
    pub fn label(self, book: &Book) -> String {
        match self {
            Part::OpeningCredits => "opening credits".into(),
            Part::Chapter(i) => format!("chapter {}", book.chapters[i].index),
            Part::ClosingCredits => "closing credits".into(),
        }
    }

    /// Directory name under `audio/`, e.g. `001-the-fog` (from the chapter's
    /// text file name, so it matches `chapters/001-the-fog.txt`).
    pub fn stem(self, book: &Book) -> String {
        match self {
            Part::OpeningCredits => OPENING_STEM.into(),
            Part::ClosingCredits => CLOSING_STEM.into(),
            Part::Chapter(i) => {
                let c = &book.chapters[i];
                c.text_file
                    .as_deref()
                    .and_then(|f| Path::new(f).file_stem())
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{:03}-{}", c.index, slugify(&c.title, 50)))
            }
        }
    }

    pub fn segments(self, book: &Book) -> &[Segment] {
        match (self, &book.credits) {
            (Part::Chapter(i), _) => &book.chapters[i].segments,
            (Part::OpeningCredits, Some(c)) => &c.opening,
            (Part::ClosingCredits, Some(c)) => &c.closing,
            _ => &[],
        }
    }

    fn segments_mut(self, book: &mut Book) -> &mut [Segment] {
        match (self, &mut book.credits) {
            (Part::Chapter(i), _) => &mut book.chapters[i].segments,
            (Part::OpeningCredits, Some(c)) => &mut c.opening,
            (Part::ClosingCredits, Some(c)) => &mut c.closing,
            _ => &mut [],
        }
    }
}

/// Every part of the finished audiobook, in playing order.
pub fn all_parts(book: &Book) -> Vec<Part> {
    let mut parts = vec![Part::OpeningCredits];
    parts.extend((0..book.chapters.len()).map(Part::Chapter));
    parts.push(Part::ClosingCredits);
    parts
}

fn selected_parts(book: &Book, opts: &SynthOptions) -> Vec<Part> {
    all_parts(book)
        .into_iter()
        .filter(|p| match p {
            Part::Chapter(i) => opts
                .chapters
                .as_ref()
                .is_none_or(|sel| sel.contains(&book.chapters[*i].index)),
            _ => true,
        })
        .collect()
}

/// Relative audio path for a segment, e.g. `audio/001-chapter-1/0003.flac`.
fn segment_path(stem: &str, segment: usize, format: &str) -> String {
    format!("audio/{stem}/{segment:04}.{format}")
}

struct Job {
    label: String,
    segment: usize,
    text: String,
    rel_path: String,
}

fn collect_jobs(book: &Book, dir: &Path, opts: &SynthOptions) -> Vec<Job> {
    let mut jobs = Vec::new();
    for part in selected_parts(book, opts) {
        let stem = part.stem(book);
        for (si, seg) in part.segments(book).iter().enumerate() {
            let rel_path = segment_path(&stem, si + 1, &opts.format);
            if !opts.force && existing_audio(dir, &rel_path, seg).is_some() {
                continue;
            }
            jobs.push(Job {
                label: part.label(book),
                segment: si + 1,
                text: seg.text.clone(),
                rel_path,
            });
        }
    }
    jobs
}

/// `audio/001-x/0016.flac` -> `audio/001-x/0016.txt`
fn text_path(rel_path: &str) -> String {
    match rel_path.rsplit_once('.') {
        Some((stem, _)) => format!("{stem}.txt"),
        None => format!("{rel_path}.txt"),
    }
}

/// Moderation split parts of a segment (`SSSS-01.ext`, `SSSS-02.ext`, ...).
fn find_parts(dir: &Path, rel_path: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for n in 1.. {
        let p = part_path(rel_path, n);
        if !dir.join(&p).is_file() {
            break;
        }
        parts.push(p);
    }
    parts
}

/// Audio files already made for `seg` from its current text, if any.
fn existing_audio(dir: &Path, rel_path: &str, seg: &Segment) -> Option<Vec<String>> {
    let made_from = fs::read_to_string(dir.join(text_path(rel_path))).ok();
    let matches = match &made_from {
        Some(text) => *text == seg.text,
        // Audio from before text files were written: trust what book.json
        // recorded for this exact segment.
        None => seg.audio_file.as_deref() == Some(rel_path) || !seg.audio_parts.is_empty(),
    };
    if !matches {
        return None;
    }
    if dir.join(rel_path).is_file() {
        return Some(vec![rel_path.to_string()]);
    }
    let parts = find_parts(dir, rel_path);
    (!parts.is_empty()).then_some(parts)
}

/// Audio for one job: a single file, or several when moderation forced a split.
enum Rendered {
    Whole,
    Parts(Vec<String>),
}

#[derive(Default)]
struct Pieces {
    audio: Vec<Vec<u8>>,
    /// (sentence, violated categories)
    blocked: Vec<(String, String)>,
}

/// Synthesise `text`. If moderation blocks it, split it in half at a
/// sentence boundary and retry each half, down to single sentences, so only
/// the sentences that are really blocked are lost. Book passages are often
/// flagged out of context and pass when sent in smaller pieces.
fn speak(
    backend: &dyn SpeechBackend,
    opts: &SynthOptions,
    text: &str,
    out: &mut Pieces,
) -> Result<(), SpeechError> {
    let req = SpeechRequest {
        model: &opts.model,
        input: text,
        voice_id: opts.voice.as_deref(),
        response_format: &opts.format,
    };
    match backend.speech(&req) {
        Ok(audio) => {
            out.audio.push(audio);
            Ok(())
        }
        Err(SpeechError::Blocked(categories)) => {
            let sentences = split_sentences(text);
            if sentences.len() <= 1 {
                out.blocked.push((text.to_string(), categories));
                return Ok(());
            }
            let mid = sentences.len() / 2;
            speak(backend, opts, &sentences[..mid].join(" "), out)?;
            speak(backend, opts, &sentences[mid..].join(" "), out)
        }
        Err(e) => Err(e),
    }
}

fn excerpt(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "..."
    }
}

/// `audio/001-x/0016.flac` -> `audio/001-x/0016-02.flac`
fn part_path(rel_path: &str, n: usize) -> String {
    match rel_path.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}-{n:02}.{ext}"),
        None => format!("{rel_path}-{n:02}"),
    }
}

fn render(
    backend: &dyn SpeechBackend,
    opts: &SynthOptions,
    dir: &Path,
    job: &Job,
) -> Result<Rendered, SpeechError> {
    let mut pieces = Pieces::default();
    speak(backend, opts, &job.text, &mut pieces)?;
    if !pieces.blocked.is_empty() {
        let list: Vec<String> = pieces
            .blocked
            .iter()
            .map(|(s, c)| format!("({c}) \"{}\"", excerpt(s, 200)))
            .collect();
        return Err(SpeechError::Failed(format!(
            "blocked by Mistral moderation: {}; reword it in book.json (segment text) and re-run, or assemble with --skip-missing",
            list.join("; ")
        )));
    }
    let io = |e: anyhow::Error| SpeechError::Failed(format!("{e:#}"));
    // Remove audio from an earlier run of this segment number.
    let _ = fs::remove_file(dir.join(text_path(&job.rel_path)));
    let _ = fs::remove_file(dir.join(&job.rel_path));
    for p in find_parts(dir, &job.rel_path) {
        let _ = fs::remove_file(dir.join(p));
    }

    let rendered = if pieces.audio.len() == 1 {
        write_atomic(&dir.join(&job.rel_path), &pieces.audio[0]).map_err(io)?;
        Rendered::Whole
    } else {
        let mut paths = Vec::new();
        for (n, audio) in pieces.audio.iter().enumerate() {
            let p = part_path(&job.rel_path, n + 1);
            write_atomic(&dir.join(&p), audio).map_err(io)?;
            paths.push(p);
        }
        Rendered::Parts(paths)
    };
    // Written last: marks the audio as complete for this text.
    write_atomic(&dir.join(text_path(&job.rel_path)), job.text.as_bytes()).map_err(io)?;
    Ok(rendered)
}

fn credit_texts(c: &Credits) -> Vec<&str> {
    c.opening
        .iter()
        .chain(&c.closing)
        .map(|s| s.text.as_str())
        .collect()
}

/// Put up-to-date credits on the book. Returns true if their text changed
/// (so existing credit audio is stale).
fn refresh_credits(book: &mut Book, info: &TtsInfo) -> bool {
    let fresh = credits::build(&book.metadata, info);
    let changed = book
        .credits
        .as_ref()
        .is_none_or(|old| credit_texts(old) != credit_texts(&fresh));
    if changed {
        book.credits = Some(fresh);
    }
    changed
}

/// Segments and characters that a run with these options would send.
pub fn plan(book: &Book, dir: &Path, opts: &SynthOptions) -> (usize, usize) {
    let mut book = book.clone();
    refresh_credits(&mut book, &opts.tts_info());
    let jobs = collect_jobs(&book, dir, opts);
    (
        jobs.len(),
        jobs.iter().map(|j| j.text.chars().count()).sum(),
    )
}

/// Synthesise the selected chapters plus the credits, and record the audio
/// files in `book`. The caller saves `book.json`.
pub fn synthesize(
    book: &mut Book,
    dir: &Path,
    backend: &dyn SpeechBackend,
    opts: &SynthOptions,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<SynthReport> {
    if !FORMATS.contains(&opts.format.as_str()) {
        bail!(
            "unsupported format '{}' (expected one of {})",
            opts.format,
            FORMATS.join(", ")
        );
    }
    let info = opts.tts_info();
    if let Some(prev) = &book.tts
        && *prev != info
        && !opts.force
        && has_audio(book)
    {
        bail!(
            "existing audio was made with model '{}', voice '{}', format '{}'; use --force to regenerate with the new settings",
            prev.model,
            prev.voice.as_deref().unwrap_or("default"),
            prev.format
        );
    }
    // Changed credit text is detected through the per-segment text files.
    refresh_credits(book, &info);

    let parts = selected_parts(book, opts);
    let total_segments: usize = parts.iter().map(|p| p.segments(book).len()).sum();
    let jobs = collect_jobs(book, dir, opts);
    let mut report = SynthReport {
        skipped: total_segments - jobs.len(),
        ..Default::default()
    };

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let fatal: Mutex<Option<String>> = Mutex::new(None);
    let results: Mutex<Vec<(usize, Result<Rendered, String>)>> = Mutex::new(Vec::new());

    thread::scope(|scope| {
        for _ in 0..opts.concurrency.clamp(1, 16) {
            scope.spawn(|| {
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let outcome = match render(backend, opts, dir, job) {
                        Ok(r) => Ok(r),
                        Err(SpeechError::Fatal(m)) => {
                            abort.store(true, Ordering::Relaxed);
                            fatal.lock().unwrap().get_or_insert(m.clone());
                            Err(m)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let status = match &outcome {
                        Ok(Rendered::Whole) => "ok".to_string(),
                        Ok(Rendered::Parts(p)) => format!(
                            "ok (blocked by moderation as a whole; synthesised in {} parts)",
                            p.len()
                        ),
                        Err(e) => format!("FAILED: {e}"),
                    };
                    progress(&format!(
                        "[{n}/{}] {} segment {} {status}",
                        jobs.len(),
                        job.label,
                        job.segment
                    ));
                    results.lock().unwrap().push((i, outcome));
                }
            });
        }
    });

    for (i, outcome) in results.into_inner().unwrap() {
        let job = &jobs[i];
        match outcome {
            Ok(rendered) => {
                report.synthesized += 1;
                report.characters += job.text.chars().count();
                if let Rendered::Parts(_) = rendered {
                    report.split_by_moderation += 1;
                }
            }
            Err(e) => report.failed.push((job.label.clone(), job.segment, e)),
        }
    }
    report.failed.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));

    // Record every audio file that exists for the selected parts.
    for part in parts {
        let stem = part.stem(book);
        for (si, seg) in part.segments_mut(book).iter_mut().enumerate() {
            let rel = segment_path(&stem, si + 1, &opts.format);
            let found = existing_audio(dir, &rel, seg);
            // Backfill the text file for audio recorded before they existed.
            if found.is_some() && !dir.join(text_path(&rel)).is_file() {
                let _ = fs::write(dir.join(text_path(&rel)), &seg.text);
            }
            match found {
                Some(files) if files.len() == 1 && files[0] == rel => {
                    seg.audio_file = Some(rel);
                    seg.audio_parts.clear();
                }
                Some(files) => {
                    seg.audio_file = None;
                    seg.audio_parts = files;
                }
                None => {
                    seg.audio_file = None;
                    seg.audio_parts.clear();
                }
            }
        }
    }
    if report.synthesized > 0 || report.skipped > 0 {
        book.tts = Some(info);
    }

    if let Some(m) = fatal.into_inner().unwrap() {
        bail!("{m}");
    }
    Ok(report)
}

fn has_audio(book: &Book) -> bool {
    book.chapters
        .iter()
        .flat_map(|c| &c.segments)
        .any(|s| s.audio_file.is_some())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        bail!("API returned empty audio");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.part",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("cannot move {} into place", tmp.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Chapter;
    use std::sync::atomic::AtomicUsize;

    struct Fake {
        calls: AtomicUsize,
        fail_on: Option<&'static str>,
    }

    impl SpeechBackend for Fake {
        fn speech(&self, req: &SpeechRequest) -> Result<Vec<u8>, SpeechError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match self.fail_on {
                Some("fatal") => Err(SpeechError::Fatal("bad key".into())),
                Some(word) if req.input.contains(word) => {
                    Err(SpeechError::Blocked("hate_and_discrimination".into()))
                }
                _ => Ok(format!("audio:{}", req.input).into_bytes()),
            }
        }
    }

    fn fake(fail_on: Option<&'static str>) -> Fake {
        Fake {
            calls: AtomicUsize::new(0),
            fail_on,
        }
    }

    fn book() -> Book {
        let seg = |t: &str| Segment {
            text: t.into(),
            words: 1,
            chars: t.chars().count(),
            pause_after_ms: 600,
            audio_file: None,
            audio_parts: Vec::new(),
        };
        let mut chapters = Vec::new();
        for (i, texts) in [vec!["One.", "Alpha."], vec!["Two.", "Beta.", "Gamma."]]
            .into_iter()
            .enumerate()
        {
            let mut c = Chapter::new(i + 1, format!("Chapter {}", i + 1), vec![]);
            c.text_file = Some(format!("chapters/{:03}-chapter-{}.txt", i + 1, i + 1));
            c.segments = texts.into_iter().map(seg).collect();
            chapters.push(c);
        }
        Book {
            generator: "test".into(),
            source_file: "x.epub".into(),
            metadata: crate::model::BookMetadata {
                title: Some("The Keeper".into()),
                authors: vec!["Ingrid Solberg".into()],
                ..Default::default()
            },
            chapters,
            tts: None,
            credits: None,
            cover: None,
        }
    }

    fn opts() -> SynthOptions {
        SynthOptions {
            model: "voxtral-mini-tts-2603".into(),
            voice: Some("en_paul_neutral".into()),
            format: "flac".into(),
            concurrency: 3,
            force: false,
            chapters: None,
        }
    }

    #[test]
    fn writes_segments_credits_and_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = book();
        let backend = fake(None);
        assert_eq!(plan(&b, tmp.path(), &opts()).0, 7);
        let report = synthesize(&mut b, tmp.path(), &backend, &opts(), &|_| {}).unwrap();
        // 5 chapter segments + opening + closing credits
        assert_eq!((report.synthesized, report.skipped), (7, 0));
        let f = tmp.path().join("audio/002-chapter-2/0003.flac");
        assert_eq!(fs::read_to_string(f).unwrap(), "audio:Gamma.");
        assert_eq!(
            b.chapters[1].segments[2].audio_file.as_deref(),
            Some("audio/002-chapter-2/0003.flac")
        );
        let closing =
            fs::read_to_string(tmp.path().join("audio/999-closing-credits/0001.flac")).unwrap();
        assert!(closing.contains("personal use only"));
        assert!(b.credits.as_ref().unwrap().opening[0].audio_file.is_some());
        assert_eq!(
            b.tts.as_ref().unwrap().voice.as_deref(),
            Some("en_paul_neutral")
        );

        // Second run: nothing left to do.
        let report = synthesize(&mut b, tmp.path(), &backend, &opts(), &|_| {}).unwrap();
        assert_eq!((report.synthesized, report.skipped), (0, 7));
        assert_eq!(backend.calls.load(Ordering::Relaxed), 7);

        // Changed metadata regenerates only the credits.
        b.metadata.title = Some("A New Title".into());
        let report = synthesize(&mut b, tmp.path(), &backend, &opts(), &|_| {}).unwrap();
        assert_eq!((report.synthesized, report.skipped), (2, 5));

        // Changing the voice without --force is refused.
        let mut other = opts();
        other.voice = Some("someone-else".into());
        assert!(synthesize(&mut b, tmp.path(), &backend, &other, &|_| {}).is_err());
    }

    #[test]
    fn selects_chapters_and_reports_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = book();
        let mut o = opts();
        o.chapters = Some(vec![2]);
        let report = synthesize(&mut b, tmp.path(), &fake(Some("Beta")), &o, &|_| {}).unwrap();
        assert_eq!(report.synthesized, 4); // 2 of chapter 2 + both credits
        assert_eq!(report.failed.len(), 1);
        let (part, seg, msg) = &report.failed[0];
        assert_eq!((part.as_str(), *seg), ("chapter 2", 2));
        assert!(
            msg.contains("blocked by Mistral moderation") && msg.contains("\"Beta.\""),
            "{msg}"
        );
        assert!(
            b.chapters[0]
                .segments
                .iter()
                .all(|s| s.audio_file.is_none())
        );
        assert!(b.chapters[1].segments[1].audio_file.is_none());
    }

    #[test]
    fn moderation_blocks_are_bisected_down_to_the_sentence() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = book();
        b.chapters[0].segments[0].text = "Fine one. Fine two. The FLAGGED line. Fine three.".into();
        let mut o = opts();
        o.chapters = Some(vec![1]);
        let report = synthesize(&mut b, tmp.path(), &fake(Some("FLAGGED")), &o, &|_| {}).unwrap();
        // Only the one flagged sentence is reported.
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].2.contains("\"The FLAGGED line.\""));
        assert!(!report.failed[0].2.contains("Fine"));

        // A passage blocked as a whole but fine in halves is stored as parts.
        struct WholeOnly;
        impl SpeechBackend for WholeOnly {
            fn speech(&self, req: &SpeechRequest) -> Result<Vec<u8>, SpeechError> {
                if req.input.split(". ").count() > 2 {
                    Err(SpeechError::Blocked("hate_and_discrimination".into()))
                } else {
                    Ok(req.input.as_bytes().to_vec())
                }
            }
        }
        let report = synthesize(&mut b, tmp.path(), &WholeOnly, &o, &|_| {}).unwrap();
        assert!(report.failed.is_empty());
        assert_eq!(report.split_by_moderation, 1);
        let seg = &b.chapters[0].segments[0];
        assert!(seg.audio_file.is_none());
        assert_eq!(
            seg.audio_parts,
            vec![
                "audio/001-chapter-1/0001-01.flac",
                "audio/001-chapter-1/0001-02.flac"
            ]
        );
        let second = fs::read_to_string(tmp.path().join(&seg.audio_parts[1])).unwrap();
        assert_eq!(second, "The FLAGGED line. Fine three.");

        // The parts count as done on the next run.
        let report = synthesize(&mut b, tmp.path(), &WholeOnly, &o, &|_| {}).unwrap();
        assert_eq!(report.synthesized, 0);
        assert_eq!(b.chapters[0].segments[0].audio_parts.len(), 2);
    }

    #[test]
    fn changed_text_is_never_served_stale_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = book();
        let backend = fake(None);
        synthesize(&mut b, tmp.path(), &backend, &opts(), &|_| {}).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("audio/001-chapter-1/0001.txt")).unwrap(),
            "One."
        );

        // Re-extraction: same segment numbers, different text, nothing recorded.
        let mut fresh = book();
        fresh.chapters[0].segments[0].text = "Uno.".into();
        let report = synthesize(&mut fresh, tmp.path(), &backend, &opts(), &|_| {}).unwrap();
        assert_eq!(report.synthesized, 1, "only the changed segment is redone");
        let audio = fs::read_to_string(tmp.path().join("audio/001-chapter-1/0001.flac")).unwrap();
        assert_eq!(audio, "audio:Uno.");
    }

    #[test]
    fn accepts_audio_recorded_before_text_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = book();
        let backend = fake(None);
        synthesize(&mut b, tmp.path(), &backend, &opts(), &|_| {}).unwrap();
        fs::remove_file(tmp.path().join("audio/002-chapter-2/0001.txt")).unwrap();
        let report = synthesize(&mut b, tmp.path(), &backend, &opts(), &|_| {}).unwrap();
        assert_eq!(report.synthesized, 0);
        assert!(
            tmp.path().join("audio/002-chapter-2/0001.txt").is_file(),
            "backfilled"
        );
    }

    #[test]
    fn fatal_error_aborts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = book();
        let backend = fake(Some("fatal"));
        let err = synthesize(&mut b, tmp.path(), &backend, &opts(), &|_| {}).unwrap_err();
        assert!(err.to_string().contains("bad key"));
        assert!(
            backend.calls.load(Ordering::Relaxed) <= 3,
            "workers stop after a fatal error"
        );
    }

    #[test]
    fn parses_chapter_lists() {
        assert_eq!(parse_chapter_list("1, 3-5,8").unwrap(), vec![1, 3, 4, 5, 8]);
        assert!(parse_chapter_list("5-3").is_err());
    }
}
