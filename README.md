# TTSAudiobookGenerator

An offline Rust tool that converts PDF, EPUB and DOCX books into chaptered audiobooks (M4B / MP3) using Mistral TTS.

**Status:** all three stages work end to end: text extraction, Mistral TTS synthesis, and assembly into M4B/MP3.

```sh
tts-audiobook extract book.epub -o out          # 1. text, chapters, segments
tts-audiobook synthesize out/<book>             # 2. audio per segment (Mistral)
tts-audiobook assemble out/<book>               # 3. out/<book>/<book>.m4b
```

## Pipeline

```
 PDF / EPUB / DOCX / MD
          │  extract/      format readers → blocks (heading, paragraph, list item, scene break) + TOC + metadata
          ▼
       clean.rs           remove footnote markers, citations, URLs, ligatures, markup, emoji
          ▼
     chapters.rs          split into chapters (source TOC → headings → "Chapter X" patterns)
          ▼
 segment.rs / ssml.rs     ≤300-word plain-text TTS segments with pauses, plus SSML per chapter
          ▼
      output.rs           out/<book>/book.json, chapters/NNN-title.{txt,ssml}, cover image
```

Mistral's TTS (`voxtral-mini-tts-2603`) takes **plain text only**, at most 4000 characters per request. `book.json` therefore splits each chapter into `segments` of at most 4000 characters, each ending at a full stop. Only a single sentence longer than the limit is cut elsewhere: at a comma or similar, or else between words. Each segment has a `pause_after_ms` that the audio stage inserts as silence. The `.ssml` files are for engines that support SSML.

## Usage

Requires a Rust toolchain (stable, edition 2024).

```sh
cargo run --release -- extract path/to/book.epub -o out
```

| Option | Default | Description |
| --- | --- | --- |
| `-o, --out <DIR>` | `out` | Output root. The book is written to `<DIR>/<title-slug>/`. |
| `--max-segment-chars <N>` | `4000` | Maximum characters per TTS request. Segments end at a full stop. |
| `--no-ssml` | | Skip the `.ssml` files. |
| `--keep-all` | | Keep chapters that are skipped by default: Contents, Index, Notes and similar. |
| `--ignore-toc` | | Ignore the book's table of contents and split on headings instead. |
| `--no-sanitize` | | Keep hashes, long numbers, base64 and over-long words instead of replacing them (see below). |

The command prints a chapter table (words, estimated minutes, segment count) so you can check the result quickly.

### Output

```
out/the-keeper-of-skarvoy-light/
├── book.json            metadata + chapters + TTS segments (contract for later stages)
├── cover.jpg            EPUB only, when the book has a cover
└── chapters/
    ├── 001-part-one-chapter-1-the-fog.txt
    ├── 001-part-one-chapter-1-the-fog.ssml
    └── ...
```

`book.json` (abridged):

```json
{
  "generator": "tts-audiobook-generator 0.1.0",
  "source_file": "sample.epub",
  "metadata": { "title": "The Keeper of Skarvøy Light", "authors": ["Ingrid Solberg"], "language": "en", ... },
  "chapters": [
    {
      "index": 1,
      "title": "Part One — Chapter 1: The Fog",
      "word_count": 360,
      "est_duration_secs": 139,
      "text_file": "chapters/001-part-one-chapter-1-the-fog.txt",
      "ssml_file": "chapters/001-part-one-chapter-1-the-fog.ssml",
      "segments": [
        { "text": "Part One — Chapter 1: The Fog.", "words": 7, "chars": 30, "pause_after_ms": 1500 },
        { "text": "The fog came in from the west ...", "words": 360, "chars": 1950, "pause_after_ms": 600 }
      ]
    }
  ]
}
```

## Synthesising audio (Mistral TTS)

Put your API key in the environment or in a `.env` file in the working directory (or a parent directory). See [.env.example](.env.example). Real environment variables take precedence over `.env`, and `.env` is git-ignored.

```sh
MISTRAL_API_KEY=your-key
MISTRAL_TTS_VOICE=            # optional default voice id
MISTRAL_TTS_VOICE_FEMALE=     # optional, used with --author-gender female
MISTRAL_TTS_VOICE_MALE=       # optional, used with --author-gender male
```

```sh
tts-audiobook voices                                               # list voice ids for your key
tts-audiobook synthesize out/<book> --chapters 1 --dry-run         # segments/characters, no API calls
tts-audiobook synthesize out/<book> --chapters 1                   # try one chapter first
tts-audiobook synthesize out/<book> --author-gender female         # whole book
```

| Option | Default | Description |
| --- | --- | --- |
| `--voice <ID>` | `MISTRAL_TTS_VOICE`, else API default | Voice id. Overrides `--author-gender`. |
| `--author-gender female\|male` | | Use `MISTRAL_TTS_VOICE_FEMALE` / `MISTRAL_TTS_VOICE_MALE`. |
| `--model <NAME>` | `voxtral-mini-tts-2603` | Or set `MISTRAL_TTS_MODEL`. |
| `--format <F>` | `flac` | `flac`, `wav`, `mp3`, `opus` or `pcm`. Lossless avoids double compression when the M4B is encoded. |
| `--concurrency <N>` | `4` | Parallel requests. |
| `--chapters <LIST>` | all | E.g. `1,3-5`. |
| `--force` | | Regenerate audio that already exists, e.g. after changing voice. |
| `--dry-run` | | Print what would be sent; no key needed. |

Each segment becomes `out/<book>/audio/NNN-<chapter>/SSSS.<format>`. The same run also voices the opening credits and the closing AI disclaimer (see below). `book.json` records each segment's `audio_file` and the model and voice used, under `tts`.

- **Resumable:** re-running the same command skips segments that already have audio. Each audio file has a `SSSS.txt` beside it holding the exact text it was made from, and audio is reused only if that text still matches. Re-running `extract` (which can renumber segments) therefore never reuses audio for the wrong text. Files are written via a `.part` file, so an interrupted run leaves no truncated audio.
- **Retries:** rate limits (429), server errors and network errors are retried with backoff, honouring `Retry-After`.
- **Moderation:** Mistral's guardrail sometimes blocks book passages, e.g. as `hate_and_discrimination`, usually because they are read out of context. A blocked segment is split in half at a sentence boundary and retried, repeatedly, until only the blocked sentences are left.
  - If every piece passes, the segment is stored as parts (`SSSS-01.flac`, `SSSS-02.flac`, ...) that are played back to back.
  - Otherwise the exact blocked sentence is reported. Reword it in `book.json` (that segment's `text`) and re-run, or build the book without it using `assemble --skip-missing`.
- **Other failures:** an invalid key (401) stops the run. Other errors are reported per segment and the rest continue.
- **Voice changes:** changing model, voice or format on a book with existing audio needs `--force`, so a book never mixes voices.

## Assembling the audiobook

```sh
tts-audiobook assemble out/<book>                    # out/<book>/<title>.m4b
tts-audiobook assemble out/<book> --format mp3 -o book.mp3
```

Requires `ffmpeg` and `ffprobe` on PATH, or set `FFMPEG` / `FFPROBE`.

| Option | Default | Description |
| --- | --- | --- |
| `--format m4b\|mp3` | `m4b` | M4B is AAC with iTunes audiobook tags. MP3 uses ID3v2.3 with CHAP frames. |
| `-o, --output <FILE>` | `<book>/<title>.<format>` | Output path. |
| `--bitrate <RATE>` | `64k` | Mono speech bitrate. |
| `--no-normalize` | | Skip loudness normalisation to -18 LUFS. |
| `--rebuild` | | Ignore the cached chapter audio in `<book>/build/`. |
| `--skip-missing` | | Leave out segments without audio, e.g. still blocked by moderation, and list them. The credits are always required. |

What goes into the file:

- **Chapters:** "Opening Credits", every book chapter, then "End Credits", each as a chapter marker. Segment pauses become silence, with a 2 s gap after each chapter.
- **Opening credits:** title and author, and a note that the audiobook was generated with AI for personal use.
- **End credits (disclaimer):** generated by AI with this project and Mistral's voice model (e.g. "Voxtral Mini TTS"), personal use only, not to be shared, sold or distributed, and all rights belong to the author.
- **Metadata:** title, author, album, genre "Audiobook", date, publisher, language, description and cover art. The narrator field names the AI voice. M4B files are marked as audiobooks.
- **Watermark:** the comment and copyright tags say the audio is AI-generated and for personal use only, naming the model and voice. MP3 files also carry `AI_GENERATED`, `AI_NOTICE`, `TTS_MODEL` and `TTS_VOICE` tags.

Each chapter is first joined into a lossless FLAC in `build/`. These are reused on later runs unless their segments change, so re-encoding (e.g. M4B, then MP3) is quick.

## How extraction works

- **EPUB:** reads OPF metadata, spine order, and the EPUB 3 nav (or EPUB 2 NCX) TOC, mapped to anchors. Footnote references and bodies are dropped using `epub:type`, `role` and `class`.
- **DOCX:** reads heading levels from `styles.xml`, so localised styles such as "Overskrift 1" work. Lists come from `numbering.xml` and metadata from `docProps/core.xml`. Word's own TOC, deleted text, field codes and note references are skipped.
- **PDF:** rebuilds the layout from glyph positions and font sizes:
  - headings are lines larger than the body text;
  - running headers, footers and page numbers are removed, along with footnotes (small type at the page bottom) and superscript note markers;
  - hyphenated line breaks are rejoined, and paragraphs are detected from gaps and indents;
  - PDF bookmarks are used as the TOC when present.
- **Markdown / TXT:** `#` headings, lists, `* * *` scene breaks, and YAML front matter for metadata.

### Sanitising technical text

Technical documentation contains strings that are useless when read aloud. Before chapters and segments are made, every token (ignoring surrounding punctuation) of these kinds is replaced:

| Kind | Rule | Example |
| --- | --- | --- |
| Any over-long word | longer than 32 characters | long file paths, identifiers |
| Hex, numbers, binary | longer than 12 characters, only hex digits (optional `0x`/`0b`, `-` `:` `.` separators, a run of 8+ digits) | SHA hashes, UUIDs, `0b1101001110010110`, serial numbers |
| No words | longer than 12 characters with letters and digits jumbled together (3+ switches), or base64 padding | API tokens, `SGVsbG8gV29ybGQhIQ==` |

Each token becomes `long non-textual string ending in 2 3 0 0 1 b`. The last six letters or digits are spaced so the voice spells them out. Punctuation is kept, so sentences still end where they did.

Readable mixes such as `Windows10Home`, `sha256sum`, `x86_64-linux-gnu`, IP addresses and MAC addresses are left alone. Very long compound words (German, Norwegian) over 32 characters are also replaced; use `--no-sanitize` for such books.

Chapter splitting chooses its split level automatically. "Part" pages are merged into the first chapter that follows ("Part One — Chapter 1: The Fog"). Contents, Index and Notes pages, and short front matter, are skipped.

## Development

```sh
cargo test                                   # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

The integration tests in `tests/pipeline.rs` run the same sample book in all four formats and check metadata, chapter titles, lists, scene breaks, cleanup and segment limits. The sample book's source is `tests/fixtures/sample.md`. To regenerate the PDF/EPUB/DOCX fixtures, run `tests/fixtures/generate.ps1`, which needs pandoc and pdflatex.

## Roadmap

1. ~~Text extraction, chapter splitting, TTS preprocessing~~
2. ~~Mistral TTS synthesis of segments: one configured voice, or a voice chosen by the author's gender~~
3. ~~Audio assembly with ffmpeg: M4B/MP3, chapters, metadata, cover, AI watermark and spoken disclaimer~~
4. Open AES-256 (R6) encrypted PDFs that only have an owner password.
5. Later: an inaudible audio watermark, and a separate voice for each character.
