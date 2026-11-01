//! Stage 3: build the final audiobook with ffmpeg.
//!
//! 1. Each part (opening credits, chapters, closing disclaimer) is joined
//!    into `build/<part>.flac`, inserting every segment's `pause_after_ms`
//!    as silence. These lossless files are cached between runs.
//! 2. The parts are concatenated and encoded once to M4B (AAC) or MP3, with
//!    chapter markers, book metadata, cover art, loudness normalisation and an
//!    "AI-generated, personal use only" watermark in the metadata tags.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::credits::{self, PROJECT_NAME};
use crate::model::Book;
use crate::output::slugify;
use crate::synthesize::{CLOSING_STEM, OPENING_STEM, Part, all_parts};

/// Silence after the last segment of every part.
pub const PART_GAP_MS: u32 = 2000;
const SAMPLE_RATE: u32 = 44_100;
/// Inputs per ffmpeg call; keeps the Windows command line well below its limit.
const BATCH: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    M4b,
    Mp3,
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::M4b => "m4b",
            OutputFormat::Mp3 => "mp3",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AssembleOptions {
    pub format: OutputFormat,
    /// Output file; defaults to `<book dir>/<title>.<ext>`.
    pub output: Option<PathBuf>,
    /// Audio bitrate, e.g. `64k` (mono speech).
    pub bitrate: String,
    /// EBU R128 loudness normalisation to -18 LUFS.
    pub normalize: bool,
    /// Rebuild cached part files even when they look up to date.
    pub rebuild: bool,
    /// Leave out segments without audio (e.g. blocked by moderation) instead
    /// of failing.
    pub skip_missing: bool,
    pub ffmpeg: String,
    pub ffprobe: String,
}

impl Default for AssembleOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::M4b,
            output: None,
            bitrate: "64k".into(),
            normalize: true,
            rebuild: false,
            skip_missing: false,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
        }
    }
}

/// A part ready for joining: chapter title plus `(audio file, pause)` pairs.
struct PartAudio {
    title: String,
    stem: String,
    items: Vec<(String, u32)>,
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| {
        format!(
            "cannot run {:?} (is ffmpeg installed and on PATH?)",
            cmd.get_program()
        )
    })?;
    if !out.status.success() {
        bail!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ffmpeg(opts: &AssembleOptions, cwd: &Path) -> Command {
    let mut c = Command::new(&opts.ffmpeg);
    c.current_dir(cwd)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    c
}

/// Duration of an audio file in milliseconds.
fn probe_ms(opts: &AssembleOptions, path: &Path) -> Result<u64> {
    let out = run(Command::new(&opts.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path))?;
    let secs: f64 = out
        .trim()
        .parse()
        .with_context(|| format!("ffprobe gave no duration for {}", path.display()))?;
    Ok((secs * 1000.0).round() as u64)
}

/// Collect each part's audio. Missing audio is an error unless
/// `skip_missing` is set, in which case it is left out and reported in the
/// returned warnings. The credits are always required.
fn gather_parts(
    book: &Book,
    dir: &Path,
    skip_missing: bool,
) -> Result<(Vec<PartAudio>, Vec<String>)> {
    if book.credits.is_none() {
        bail!("book has no credits audio yet; run `synthesize` first");
    }
    let exists = |f: &String| dir.join(f).is_file();
    let mut parts = Vec::new();
    let mut missing = Vec::new();
    for part in all_parts(book) {
        let segments = part.segments(book);
        let mut items = Vec::new();
        let mut absent = Vec::new();
        for (i, seg) in segments.iter().enumerate() {
            let files: Vec<String> = match &seg.audio_file {
                Some(f) if exists(f) => vec![f.clone()],
                _ if !seg.audio_parts.is_empty() && seg.audio_parts.iter().all(exists) => {
                    seg.audio_parts.clone()
                }
                _ => {
                    absent.push(i + 1);
                    continue;
                }
            };
            let pause = if i + 1 == segments.len() {
                seg.pause_after_ms.max(PART_GAP_MS)
            } else {
                seg.pause_after_ms
            };
            // Moderation split parts play back to back; the pause follows the last.
            let last = files.len() - 1;
            for (n, f) in files.into_iter().enumerate() {
                items.push((f, if n == last { pause } else { 0 }));
            }
        }
        let is_credits = !matches!(part, Part::Chapter(_));
        if !absent.is_empty() || segments.is_empty() {
            let list = absent
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            missing.push(format!("{} (segments {list})", part.label(book)));
            // Fails below, unless skipping a chapter (partly or wholly) is allowed.
            if !skip_missing || is_credits || items.is_empty() {
                continue;
            }
        }
        if let Some(last) = items.last_mut() {
            last.1 = last.1.max(PART_GAP_MS);
        }
        let title = match part {
            Part::OpeningCredits => "Opening Credits".to_string(),
            Part::ClosingCredits => "End Credits".to_string(),
            Part::Chapter(i) => book.chapters[i].title.clone(),
        };
        parts.push(PartAudio {
            title,
            stem: part.stem(book),
            items,
        });
    }
    let credits_missing = parts.first().is_none_or(|p| p.stem != OPENING_STEM)
        || parts.last().is_none_or(|p| p.stem != CLOSING_STEM);
    if !missing.is_empty() && (!skip_missing || credits_missing) {
        bail!(
            "audio is missing for: {}; run `synthesize` (without --chapters) to fill the gaps, or use --skip-missing to leave them out",
            missing.join(", ")
        );
    }
    Ok((parts, missing))
}

/// Join audio files (paths relative to `cwd`) with trailing silence after
/// each, normalised to 44.1 kHz mono, into a FLAC file.
fn join_with_pauses(
    opts: &AssembleOptions,
    cwd: &Path,
    items: &[(String, u32)],
    out: &Path,
) -> Result<()> {
    if items.len() > BATCH {
        // Join in batches, then join the batches.
        let mut batch_files = Vec::new();
        for (n, chunk) in items.chunks(BATCH).enumerate() {
            let file = out.with_extension(format!("batch{n}.flac"));
            join_with_pauses(opts, cwd, chunk, &file)?;
            batch_files.push((file.to_string_lossy().into_owned(), 0));
        }
        join_with_pauses(opts, cwd, &batch_files, out)?;
        for (f, _) in batch_files {
            let _ = fs::remove_file(f);
        }
        return Ok(());
    }

    let mut cmd = ffmpeg(opts, cwd);
    let mut graph = String::new();
    for (i, (file, pause)) in items.iter().enumerate() {
        cmd.arg("-i").arg(file);
        graph.push_str(&format!(
            "[{i}:a]aresample={SAMPLE_RATE},aformat=sample_fmts=s16:channel_layouts=mono"
        ));
        if *pause > 0 {
            graph.push_str(&format!(",apad=pad_dur={:.3}", *pause as f64 / 1000.0));
        }
        graph.push_str(&format!("[a{i}];"));
    }
    for i in 0..items.len() {
        graph.push_str(&format!("[a{i}]"));
    }
    graph.push_str(&format!("concat=n={}:v=0:a=1[out]", items.len()));
    let tmp = out.with_extension("flac.part");
    cmd.args([
        "-filter_complex",
        &graph,
        "-map",
        "[out]",
        "-c:a",
        "flac",
        "-f",
        "flac",
    ])
    .arg(&tmp);
    run(&mut cmd)?;
    fs::rename(&tmp, out)?;
    Ok(())
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Escape a value for ffmpeg's FFMETADATA format.
fn meta_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// FFMETADATA file with book tags, the watermark and chapter markers.
fn metadata_file(book: &Book, format: OutputFormat, chapters: &[(String, u64, u64)]) -> String {
    let m = &book.metadata;
    let tts = book.tts.clone().unwrap_or_else(|| crate::model::TtsInfo {
        provider: "mistral".into(),
        model: crate::mistral::DEFAULT_MODEL.into(),
        voice: None,
        format: "flac".into(),
    });
    let title = m.title.clone().unwrap_or_else(|| "Audiobook".into());
    let authors = m.authors.join(", ");
    let mark = credits::watermark(m, &tts);
    let narrator = format!(
        "AI voice: Mistral {} ({})",
        tts.model,
        tts.voice.as_deref().unwrap_or("default voice")
    );

    let mut tags: Vec<(&str, String)> = vec![
        ("title", title.clone()),
        ("album", title),
        ("artist", authors.clone()),
        ("album_artist", authors.clone()),
        ("composer", narrator),
        ("genre", "Audiobook".into()),
        ("comment", mark.clone()),
        (
            "description",
            m.description
                .clone()
                .map_or(mark.clone(), |d| format!("{d}\n\n{mark}")),
        ),
        (
            "copyright",
            format!(
                "© {}. AI-generated narration for personal use only.",
                credits::author_list(&m.authors)
            ),
        ),
        ("encoded_by", PROJECT_NAME.into()),
    ];
    if let Some(date) = &m.published {
        tags.push(("date", date.chars().take(10).collect()));
    }
    if let Some(p) = &m.publisher {
        tags.push(("publisher", p.clone()));
    }
    if let Some(l) = &m.language {
        tags.push(("language", l.clone()));
    }
    match format {
        // iTunes "stik" media kind 2 = audiobook.
        OutputFormat::M4b => tags.push(("media_type", "2".into())),
        // MP3 keeps arbitrary keys as ID3 TXXX frames.
        OutputFormat::Mp3 => {
            tags.push(("AI_GENERATED", "true".into()));
            tags.push(("AI_NOTICE", mark));
            tags.push(("TTS_MODEL", tts.model.clone()));
            tags.push((
                "TTS_VOICE",
                tts.voice.clone().unwrap_or_else(|| "default".into()),
            ));
        }
    }

    let mut out = String::from(";FFMETADATA1\n");
    for (k, v) in tags {
        out.push_str(&format!("{k}={}\n", meta_escape(&v)));
    }
    for (title, start, end) in chapters {
        out.push_str(&format!(
            "\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\ntitle={}\n",
            meta_escape(title)
        ));
    }
    out
}

/// Build the audiobook and return the path of the written file.
pub fn assemble(
    book: &Book,
    dir: &Path,
    opts: &AssembleOptions,
    progress: &dyn Fn(&str),
) -> Result<PathBuf> {
    run(Command::new(&opts.ffmpeg).arg("-version"))
        .context("ffmpeg is required for assembly (install it or set FFMPEG)")?;
    let dir = &std::path::absolute(dir)?;
    let (parts, skipped) = gather_parts(book, dir, opts.skip_missing)?;
    for s in &skipped {
        progress(&format!("warning: no audio, left out: {s}"));
    }
    let build = dir.join("build");
    fs::create_dir_all(&build)?;

    // 1. One lossless file per part, rebuilt only when its inputs change.
    let mut part_files = Vec::new();
    for (n, part) in parts.iter().enumerate() {
        let file = build.join(format!("{}.flac", part.stem));
        let sig_file = file.with_extension("sig");
        let signature: String = part
            .items
            .iter()
            .map(|(f, p)| format!("{f}\t{p}\n"))
            .collect();
        let newest_input = part
            .items
            .iter()
            .filter_map(|(f, _)| mtime(&dir.join(f)))
            .max();
        let fresh = !opts.rebuild
            && fs::read_to_string(&sig_file).ok().as_deref() == Some(signature.as_str())
            && mtime(&file).is_some_and(|out| newest_input.is_none_or(|inp| out >= inp));
        if fresh {
            progress(&format!(
                "[{}/{}] {} (cached)",
                n + 1,
                parts.len(),
                part.title
            ));
        } else {
            progress(&format!(
                "[{}/{}] joining {} segments: {}",
                n + 1,
                parts.len(),
                part.items.len(),
                part.title
            ));
            join_with_pauses(opts, dir, &part.items, &file)?;
            fs::write(&sig_file, signature)?;
        }
        part_files.push(file);
    }

    // 2. Chapter markers from the part durations.
    let mut chapters = Vec::new();
    let mut t = 0;
    for (part, file) in parts.iter().zip(&part_files) {
        let ms = probe_ms(opts, file)?;
        chapters.push((part.title.clone(), t, t + ms));
        t += ms;
    }

    let list: String = std::iter::once("ffconcat version 1.0\n".to_string())
        .chain(part_files.iter().map(|f| {
            let name = f
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace('\'', "'\\''");
            format!("file '{name}'\n")
        }))
        .collect();
    fs::write(build.join("parts.ffconcat"), list)?;
    fs::write(
        build.join("metadata.txt"),
        metadata_file(book, opts.format, &chapters),
    )?;

    // 3. Final encode.
    let output = match &opts.output {
        Some(p) => p.clone(),
        None => dir.join(format!(
            "{}.{}",
            slugify(book.metadata.title.as_deref().unwrap_or("audiobook"), 80),
            opts.format.extension()
        )),
    };
    let output = std::path::absolute(&output)?;
    let tmp = output.with_extension(format!("{}.part", opts.format.extension()));
    progress(&format!(
        "Encoding {} ({}h{:02}m) ...",
        output.display(),
        t / 3_600_000,
        (t % 3_600_000) / 60_000
    ));

    let mut cmd = ffmpeg(opts, &build);
    cmd.args([
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        "parts.ffconcat",
        "-i",
        "metadata.txt",
    ]);
    let cover = book
        .metadata
        .cover_file
        .as_deref()
        .map(|c| dir.join(c))
        .filter(|c| c.is_file());
    if let Some(c) = &cover {
        cmd.arg("-i").arg(std::path::absolute(c)?);
    }
    cmd.args(["-map", "0:a", "-map_metadata", "1", "-map_chapters", "1"]);
    if cover.is_some() {
        cmd.args([
            "-map",
            "2:v",
            "-c:v",
            "copy",
            "-disposition:v:0",
            "attached_pic",
        ]);
    }
    if opts.normalize {
        cmd.args(["-af", "loudnorm=I=-18:TP=-2:LRA=11"]);
    }
    cmd.args([
        "-ar",
        &SAMPLE_RATE.to_string(),
        "-ac",
        "1",
        "-b:a",
        &opts.bitrate,
    ]);
    match opts.format {
        OutputFormat::M4b => cmd.args(["-c:a", "aac", "-movflags", "+faststart", "-f", "ipod"]),
        OutputFormat::Mp3 => cmd.args(["-c:a", "libmp3lame", "-id3v2_version", "3", "-f", "mp3"]),
    };
    cmd.arg(&tmp);
    run(&mut cmd)?;
    fs::rename(&tmp, &output).with_context(|| format!("cannot write {}", output.display()))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_metadata_values() {
        assert_eq!(meta_escape("a=b; c#d\\e\nf"), "a\\=b\\; c\\#d\\\\e\\\nf");
    }
}
