//! End-to-end: extract the sample book, "synthesise" it with a fake TTS
//! backend that returns real FLAC tones, then assemble M4B and MP3 files with
//! ffmpeg and inspect them with ffprobe. Skipped when ffmpeg is not installed.

use std::path::PathBuf;
use std::process::Command;

use tts_audiobook_generator::assemble::{AssembleOptions, OutputFormat, assemble};
use tts_audiobook_generator::mistral::{SpeechBackend, SpeechError, SpeechRequest};
use tts_audiobook_generator::sentences::split_sentences;
use tts_audiobook_generator::synthesize::{SynthOptions, load_book, save_book, synthesize};
use tts_audiobook_generator::{ExtractOptions, extract_book, output};

struct ToneBackend(Vec<u8>);

impl SpeechBackend for ToneBackend {
    fn speech(&self, _: &SpeechRequest) -> Result<Vec<u8>, SpeechError> {
        Ok(self.0.clone())
    }
}

fn have_ffmpeg() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|t| {
        Command::new(t)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// 0.4 s of 440 Hz at 24 kHz mono, as FLAC (like a TTS response).
fn tone() -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.4:sample_rate=24000",
            "-f",
            "flac",
            "-",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout
}

fn probe(path: &PathBuf) -> serde_json::Value {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_chapters",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn assembles_m4b_and_mp3_with_chapters_and_notices() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg/ffprobe not found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.md");
    let mut book = extract_book(
        &sample,
        &ExtractOptions {
            // Tiny segments push chapter 1 past the 100-input batch size.
            max_segment_chars: 15,
            ..Default::default()
        },
    )
    .unwrap();
    let dir = output::write_book(&mut book, tmp.path(), false).unwrap();

    let (mut book, dir) = load_book(&dir).unwrap();
    let opts = SynthOptions {
        model: "voxtral-mini-tts-2603".into(),
        voice: Some("test_voice".into()),
        format: "flac".into(),
        concurrency: 4,
        force: false,
        chapters: None,
    };
    let report = synthesize(&mut book, &dir, &ToneBackend(tone()), &opts, &|_| {}).unwrap();
    assert!(report.failed.is_empty());
    save_book(&book, &dir).unwrap();

    let segments: usize = book
        .chapters
        .iter()
        .map(|c| c.segments.len())
        .sum::<usize>()
        + 2;
    let pauses_ms: u64 = book
        .chapters
        .iter()
        .flat_map(|c| &c.segments)
        .chain(
            book.credits
                .iter()
                .flat_map(|c| c.opening.iter().chain(&c.closing)),
        )
        .map(|s| s.pause_after_ms as u64)
        .sum();

    let m4b = assemble(&book, &dir, &AssembleOptions::default(), &|_| {}).unwrap();
    assert_eq!(m4b.extension().unwrap(), "m4b");
    let info = probe(&m4b);

    let chapters = info["chapters"].as_array().unwrap();
    let titles: Vec<&str> = chapters
        .iter()
        .map(|c| c["tags"]["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        vec![
            "Opening Credits",
            "Part One — Chapter 1: The Fog",
            "Chapter 2: The Storm",
            "Part Two — Chapter 3: The Ship",
            "Chapter 4: The Return",
            "End Credits"
        ]
    );
    // Chapters are contiguous.
    for w in chapters.windows(2) {
        assert_eq!(w[0]["end_time"], w[1]["start_time"]);
    }

    let tags = &info["format"]["tags"];
    assert_eq!(tags["title"], "The Keeper of Skarvøy Light");
    assert_eq!(tags["artist"], "Ingrid Solberg");
    assert_eq!(tags["genre"], "Audiobook");
    assert!(
        tags["comment"]
            .as_str()
            .unwrap()
            .contains("For personal use only")
    );
    assert!(
        tags["comment"]
            .as_str()
            .unwrap()
            .contains("voxtral-mini-tts-2603")
    );
    assert!(
        tags["copyright"]
            .as_str()
            .unwrap()
            .contains("Ingrid Solberg")
    );

    let stream = &info["streams"][0];
    assert_eq!(stream["codec_name"], "aac");
    assert_eq!(stream["sample_rate"], "44100");
    assert_eq!(stream["channels"], 1);

    // Total = tones + pauses (+ the longer gap at the end of each part).
    let duration: f64 = info["format"]["duration"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let expected = segments as f64 * 0.4 + pauses_ms as f64 / 1000.0;
    assert!(
        duration >= expected - 0.5 && duration <= expected + 6.0 * 2.0 + 1.0,
        "duration {duration} vs expected about {expected}"
    );

    // MP3 reuses the cached part files and carries the notice as ID3 tags.
    let mp3_opts = AssembleOptions {
        format: OutputFormat::Mp3,
        ..Default::default()
    };
    let mp3 = assemble(&book, &dir, &mp3_opts, &|_| {}).unwrap();
    let info = probe(&mp3);
    assert_eq!(info["chapters"].as_array().unwrap().len(), 6);
    let tags = &info["format"]["tags"];
    assert_eq!(tags["AI_GENERATED"], "true");
    assert_eq!(tags["TTS_VOICE"], "test_voice");
}

#[test]
fn refuses_incomplete_audio() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.md");
    let mut book = extract_book(&sample, &ExtractOptions::default()).unwrap();
    let dir = output::write_book(&mut book, tmp.path(), false).unwrap();
    let (book, dir) = load_book(&dir).unwrap();
    let err = assemble(&book, &dir, &AssembleOptions::default(), &|_| {}).unwrap_err();
    assert!(err.to_string().contains("synthesize"), "{err}");
}

/// Blocks "Mr. Moen did not mind." unless sent on its own (rescued by
/// splitting) and always blocks the sentence about the gulls.
struct ModeratedTones(Vec<u8>);

impl SpeechBackend for ModeratedTones {
    fn speech(&self, req: &SpeechRequest) -> Result<Vec<u8>, SpeechError> {
        let blocked = req.input.contains("gulls stopped arguing")
            || (req.input.contains("Moen did not mind") && split_sentences(req.input).len() > 1);
        if blocked {
            Err(SpeechError::Blocked("hate_and_discrimination".into()))
        } else {
            Ok(self.0.clone())
        }
    }
}

#[test]
fn moderation_split_parts_and_skip_missing() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.md");
    let mut book = extract_book(&sample, &ExtractOptions::default()).unwrap();
    let dir = output::write_book(&mut book, tmp.path(), false).unwrap();
    let (mut book, dir) = load_book(&dir).unwrap();
    let opts = SynthOptions {
        model: "voxtral-mini-tts-2603".into(),
        voice: None,
        format: "flac".into(),
        concurrency: 2,
        force: false,
        chapters: None,
    };
    let report = synthesize(&mut book, &dir, &ModeratedTones(tone()), &opts, &|_| {}).unwrap();
    save_book(&book, &dir).unwrap();

    // Chapter 1 has a single body segment containing both sentences: the
    // gull sentence stays blocked, so the segment has no audio.
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert!(report.failed[0].2.contains("gulls stopped arguing"));
    assert!(!report.failed[0].2.contains("Moen"));

    let err = assemble(&book, &dir, &AssembleOptions::default(), &|_| {}).unwrap_err();
    assert!(err.to_string().contains("--skip-missing"), "{err}");

    let opts = AssembleOptions {
        skip_missing: true,
        ..Default::default()
    };
    let warnings = std::sync::Mutex::new(Vec::new());
    let m4b = assemble(&book, &dir, &opts, &|l| {
        warnings.lock().unwrap().push(l.to_string())
    })
    .unwrap();
    assert!(
        warnings
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("left out: chapter 1"))
    );
    let info = probe(&m4b);
    assert_eq!(info["chapters"].as_array().unwrap().len(), 6);

    // Rewording the blocked text (as the error suggests) and re-running
    // fills the gap; the Moen sentence is then rescued as split parts.
    let seg = book.chapters[0]
        .segments
        .iter_mut()
        .find(|s| s.text.contains("gulls stopped arguing"))
        .unwrap();
    seg.text = seg
        .text
        .replace("gulls stopped arguing", "gulls fell silent");
    let report = synthesize(
        &mut book,
        &dir,
        &ModeratedTones(tone()),
        &SynthOptions {
            model: "voxtral-mini-tts-2603".into(),
            voice: None,
            format: "flac".into(),
            concurrency: 2,
            force: false,
            chapters: None,
        },
        &|_| {},
    )
    .unwrap();
    assert!(report.failed.is_empty());
    assert_eq!(report.split_by_moderation, 1);
    assert!(
        book.chapters[0]
            .segments
            .iter()
            .any(|s| s.audio_parts.len() > 1)
    );
    let m4b = assemble(&book, &dir, &AssembleOptions::default(), &|_| {}).unwrap();
    assert_eq!(probe(&m4b)["chapters"].as_array().unwrap().len(), 6);
}
