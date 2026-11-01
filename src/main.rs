use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tts_audiobook_generator::assemble::{self, AssembleOptions, OutputFormat};
use tts_audiobook_generator::mistral::{self, MistralClient};
use tts_audiobook_generator::model::Book;
use tts_audiobook_generator::segment::DEFAULT_MAX_SEGMENT_CHARS;
use tts_audiobook_generator::synthesize::{self as synth, SynthOptions};
use tts_audiobook_generator::{ExtractOptions, extract_book, output};

/// Default voice for every book.
const VOICE_ENV: &str = "MISTRAL_TTS_VOICE";
/// Voices picked by `--author-gender`.
const VOICE_FEMALE_ENV: &str = "MISTRAL_TTS_VOICE_FEMALE";
const VOICE_MALE_ENV: &str = "MISTRAL_TTS_VOICE_MALE";
const MODEL_ENV: &str = "MISTRAL_TTS_MODEL";

#[derive(Parser)]
#[command(
    version,
    about = "Convert PDF/EPUB/DOCX books into chaptered TTS audiobooks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Gender {
    Female,
    Male,
}

#[derive(Subcommand)]
enum Command {
    /// Extract chapters, metadata and TTS-ready text from a book.
    Extract {
        /// Input file (.pdf, .epub, .docx, .md or .txt).
        input: PathBuf,
        /// Output root; the book is written to <OUT>/<book-title-slug>/.
        #[arg(short, long, default_value = "out")]
        out: PathBuf,
        /// Maximum characters per TTS request; segments end at a full stop.
        #[arg(long, default_value_t = DEFAULT_MAX_SEGMENT_CHARS)]
        max_segment_chars: usize,
        /// Do not write SSML files.
        #[arg(long)]
        no_ssml: bool,
        /// Keep chapters that are skipped by default (Contents, Index, Notes, ...).
        #[arg(long)]
        keep_all: bool,
        /// Ignore the book's table of contents and split on headings instead.
        #[arg(long)]
        ignore_toc: bool,
        /// Keep hashes, long numbers, base64 and words over 32 characters as
        /// they are instead of replacing them with "long non-textual string
        /// ending in ...".
        #[arg(long)]
        no_sanitize: bool,
    },
    /// Generate audio for an extracted book with Mistral TTS.
    ///
    /// Reads MISTRAL_API_KEY from the environment or a .env file. Re-running
    /// the same command resumes where the previous run stopped.
    Synthesize {
        /// Book directory created by `extract` (or its book.json).
        book: PathBuf,
        /// Mistral voice id [env: MISTRAL_TTS_VOICE]. Omit to use the API default.
        #[arg(long)]
        voice: Option<String>,
        /// Pick the voice from MISTRAL_TTS_VOICE_FEMALE / MISTRAL_TTS_VOICE_MALE.
        #[arg(long, value_enum)]
        author_gender: Option<Gender>,
        /// TTS model [env: MISTRAL_TTS_MODEL] [default: voxtral-mini-tts-2603].
        #[arg(long)]
        model: Option<String>,
        /// Audio format per segment: flac, wav, mp3, opus or pcm.
        #[arg(long, default_value = "flac")]
        format: String,
        /// Parallel requests.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Only these chapters, e.g. `1,3-5`.
        #[arg(long)]
        chapters: Option<String>,
        /// Regenerate segments that already have audio.
        #[arg(long)]
        force: bool,
        /// Show what would be sent without calling the API.
        #[arg(long)]
        dry_run: bool,
    },
    /// Build the final M4B/MP3 with chapters, metadata, cover and AI notices.
    ///
    /// Requires ffmpeg (and ffprobe) on PATH, or set FFMPEG / FFPROBE.
    Assemble {
        /// Book directory created by `extract` (or its book.json).
        book: PathBuf,
        /// Output container.
        #[arg(long, value_enum, default_value = "m4b")]
        format: AudioFormat,
        /// Output file [default: <BOOK>/<title>.<format>].
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Audio bitrate (mono speech).
        #[arg(long, default_value = "64k")]
        bitrate: String,
        /// Skip loudness normalisation (-18 LUFS).
        #[arg(long)]
        no_normalize: bool,
        /// Rebuild cached chapter audio in <BOOK>/build.
        #[arg(long)]
        rebuild: bool,
        /// Leave out segments that have no audio (e.g. blocked by moderation).
        #[arg(long)]
        skip_missing: bool,
    },
    /// List the Mistral voices available to your API key.
    Voices,
}

#[derive(Clone, Copy, ValueEnum)]
enum AudioFormat {
    M4b,
    Mp3,
}

fn main() -> ExitCode {
    // A .env file (in this or a parent directory) fills in missing env vars.
    let _ = dotenvy::dotenv();
    let result = match Cli::parse().command {
        Command::Extract {
            input,
            out,
            max_segment_chars,
            no_ssml,
            keep_all,
            ignore_toc,
            no_sanitize,
        } => run_extract(
            input,
            out,
            ExtractOptions {
                max_segment_chars,
                keep_all,
                ignore_toc,
                sanitize: !no_sanitize,
            },
            !no_ssml,
        ),
        Command::Synthesize {
            book,
            voice,
            author_gender,
            model,
            format,
            concurrency,
            chapters,
            force,
            dry_run,
        } => synth_options(
            voice,
            author_gender,
            model,
            format,
            concurrency,
            chapters,
            force,
        )
        .and_then(|opts| run_synthesize(book, opts, dry_run)),
        Command::Assemble {
            book,
            format,
            output,
            bitrate,
            no_normalize,
            rebuild,
            skip_missing,
        } => run_assemble(
            book,
            AssembleOptions {
                format: match format {
                    AudioFormat::M4b => OutputFormat::M4b,
                    AudioFormat::Mp3 => OutputFormat::Mp3,
                },
                output,
                bitrate,
                normalize: !no_normalize,
                rebuild,
                skip_missing,
                ffmpeg: env_var("FFMPEG").unwrap_or_else(|| "ffmpeg".into()),
                ffprobe: env_var("FFPROBE").unwrap_or_else(|| "ffprobe".into()),
            },
        ),
        Command::Voices => run_voices(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `--voice`, else the gender-specific voice, else `MISTRAL_TTS_VOICE`,
/// else none (the API's default voice).
fn resolve_voice(voice: Option<String>, gender: Option<Gender>) -> Result<Option<String>> {
    if voice.is_some() {
        return Ok(voice);
    }
    if let Some(g) = gender {
        let name = match g {
            Gender::Female => VOICE_FEMALE_ENV,
            Gender::Male => VOICE_MALE_ENV,
        };
        return match env_var(name) {
            Some(v) => Ok(Some(v)),
            None => bail!("--author-gender needs {name} to be set (in the environment or .env)"),
        };
    }
    Ok(env_var(VOICE_ENV))
}

fn synth_options(
    voice: Option<String>,
    author_gender: Option<Gender>,
    model: Option<String>,
    format: String,
    concurrency: usize,
    chapters: Option<String>,
    force: bool,
) -> Result<SynthOptions> {
    Ok(SynthOptions {
        model: model
            .or_else(|| env_var(MODEL_ENV))
            .unwrap_or_else(|| mistral::DEFAULT_MODEL.to_string()),
        voice: resolve_voice(voice, author_gender)?,
        format: format.to_ascii_lowercase(),
        concurrency,
        force,
        chapters: chapters
            .as_deref()
            .map(synth::parse_chapter_list)
            .transpose()?,
    })
}

fn run_extract(input: PathBuf, out: PathBuf, opts: ExtractOptions, ssml: bool) -> Result<()> {
    let mut book = extract_book(&input, &opts)?;
    let dir = output::write_book(&mut book, &out, ssml)?;
    print_summary(&book);
    println!("\nWritten to {}", dir.display());
    Ok(())
}

fn run_synthesize(path: PathBuf, opts: SynthOptions, dry_run: bool) -> Result<()> {
    let (mut book, dir) = synth::load_book(&path)?;
    let (segments, chars) = synth::plan(&book, &dir, &opts);
    println!(
        "{}: {} segments ({} characters) to synthesise with {} / voice: {} / {}",
        book.metadata.title.as_deref().unwrap_or("book"),
        segments,
        chars,
        opts.model,
        opts.voice.as_deref().unwrap_or("API default"),
        opts.format
    );
    if dry_run {
        return Ok(());
    }

    let client = MistralClient::from_env()?;
    let result = synth::synthesize(&mut book, &dir, &client, &opts, &|line| println!("{line}"));
    // Record whatever was produced, even when the run stopped early.
    synth::save_book(&book, &dir)?;
    let report = result?;

    println!(
        "\nDone: {} synthesised, {} already present, {} failed ({} characters sent).",
        report.synthesized,
        report.skipped,
        report.failed.len(),
        report.characters
    );
    if report.split_by_moderation > 0 {
        println!(
            "{} segments were blocked by moderation as a whole and synthesised in smaller pieces.",
            report.split_by_moderation
        );
    }
    for (part, segment, error) in &report.failed {
        println!("  {part} segment {segment}: {error}");
    }
    if !report.failed.is_empty() {
        bail!(
            "{} segments failed; re-run the same command to retry them",
            report.failed.len()
        );
    }
    println!(
        "Audio in {}; next: tts-audiobook assemble {}",
        dir.join("audio").display(),
        path.display()
    );
    Ok(())
}

fn run_assemble(path: PathBuf, opts: AssembleOptions) -> Result<()> {
    let (book, dir) = synth::load_book(&path)?;
    let out = assemble::assemble(&book, &dir, &opts, &|line| println!("{line}"))?;
    println!(
        "
Audiobook written to {}",
        out.display()
    );
    Ok(())
}

fn run_voices() -> Result<()> {
    let voices = MistralClient::from_env()?.list_voices()?;
    println!(
        "{:<36} {:<24} {:<8} {:<8} Languages",
        "Id", "Name", "Gender", "Type"
    );
    for v in &voices {
        println!(
            "{:<36} {:<24} {:<8} {:<8} {}",
            v.id,
            v.name.as_deref().unwrap_or("-"),
            v.gender.as_deref().unwrap_or("-"),
            v.kind.as_deref().unwrap_or("-"),
            v.languages.join(", ")
        );
    }
    println!("\n{} voices", voices.len());
    Ok(())
}

fn print_summary(book: &Book) {
    let m = &book.metadata;
    println!("Title:    {}", m.title.as_deref().unwrap_or("-"));
    if let Some(sub) = &m.subtitle {
        println!("Subtitle: {sub}");
    }
    println!(
        "Authors:  {}",
        if m.authors.is_empty() {
            "-".into()
        } else {
            m.authors.join(", ")
        }
    );
    println!("Language: {}", m.language.as_deref().unwrap_or("-"));
    println!("Format:   {}\n", m.source_format);

    println!(
        "{:>3}  {:<52} {:>7} {:>6} {:>5}",
        "#", "Chapter", "Words", "Min", "Segs"
    );
    for c in &book.chapters {
        let title: String = if c.title.chars().count() > 52 {
            c.title.chars().take(49).collect::<String>() + "..."
        } else {
            c.title.clone()
        };
        println!(
            "{:>3}  {:<52} {:>7} {:>6.1} {:>5}",
            c.index,
            title,
            c.word_count,
            c.est_duration_secs as f64 / 60.0,
            c.segments.len()
        );
    }
    let words: usize = book.chapters.iter().map(|c| c.word_count).sum();
    let secs: u32 = book.chapters.iter().map(|c| c.est_duration_secs).sum();
    println!(
        "\n{} chapters, {} words, ~{}h{:02}m of narration",
        book.chapters.len(),
        words,
        secs / 3600,
        (secs % 3600) / 60
    );
}
