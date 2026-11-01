use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tts_audiobook_generator::model::Book;
use tts_audiobook_generator::segment::DEFAULT_MAX_SEGMENT_WORDS;
use tts_audiobook_generator::{ExtractOptions, extract_book, output};

#[derive(Parser)]
#[command(
    version,
    about = "Convert PDF/EPUB/DOCX books into chaptered TTS audiobooks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
        /// Maximum words per TTS segment (Mistral recommends < 300).
        #[arg(long, default_value_t = DEFAULT_MAX_SEGMENT_WORDS)]
        max_segment_words: usize,
        /// Do not write SSML files.
        #[arg(long)]
        no_ssml: bool,
        /// Keep chapters that are skipped by default (Contents, Index, Notes, ...).
        #[arg(long)]
        keep_all: bool,
        /// Ignore the book's table of contents and split on headings instead.
        #[arg(long)]
        ignore_toc: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Extract {
            input,
            out,
            max_segment_words,
            no_ssml,
            keep_all,
            ignore_toc,
        } => {
            let opts = ExtractOptions {
                max_segment_words,
                keep_all,
                ignore_toc,
            };
            let result = extract_book(&input, &opts).and_then(|mut book| {
                let dir = output::write_book(&mut book, &out, !no_ssml)?;
                Ok((book, dir))
            });
            match result {
                Ok((book, dir)) => {
                    print_summary(&book);
                    println!("\nWritten to {}", dir.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
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
