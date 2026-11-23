# TTSAudiobookGenerator

An offline Rust tool that converts PDF, EPUB and DOCX books into chaptered audiobooks (M4B / MP3) using Mistral TTS.

**Status:** all stages work end to end: text extraction, optional LLM text repair, Mistral TTS synthesis, and assembly into M4B/MP3.

```sh
tts-audiobook extract book.epub -o out          # 1. text, chapters, segments (raw/*.md)
tts-audiobook refine out/<book>                 # 2. optional: LLM repairs the text for speech (text/*.md)
tts-audiobook synthesize out/<book>             # 3. audio per segment (Mistral)
tts-audiobook assemble out/<book>               # 4. out/<book>/<book>.m4b
```

## Pipeline

```
 PDF / EPUB / DOCX / MD
          │  extract/      format readers → blocks (heading, paragraph, list item, table, figure, scene break) + TOC + metadata
          ▼
       clean.rs           remove footnote markers, citations, URLs (host only), ligatures, markup, emoji; name hashes by type
          ▼
     chapters.rs          split into chapters (source TOC → headings → "Chapter X" patterns)
          ▼
      output.rs           out/<book>/raw/NNN-title.md: the chapter as Markdown
          ▼
      refine.rs           optional: an LLM repairs raw/ into text/NNN-title.md (tables, figures, code → speech)
          ▼
 segment.rs / ssml.rs     plain-text TTS segments with pauses from text/ (or raw/), plus SSML per chapter
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
| `--max-segment-chars <N>` | `4000` | Maximum characters per TTS request. Segments end at a full stop. Keep it at 4000 or less for Voxtral. |
| `--min-segment-chars <N>` | `600` | Segments shorter than this are merged into a neighbour when the result fits. |
| `--no-ssml` | | Skip the `.ssml` files. |
| `--keep-all` | | Keep chapters that are skipped by default: Contents, Index, Notes, Glossary, Bibliography, Copyright and similar. |
| `--ignore-toc` | | Ignore the book's table of contents and split on headings instead. |
| `--no-sanitize` | | Keep hashes, long numbers, base64 and paths instead of naming their type (see below). |

Headings are spoken together with the text that follows them, so they never become tiny segments of their own. Only the chapter title is always a segment by itself, so chapter markers line up with it.

The command prints a chapter table (words, estimated minutes, segment count) so you can check the result quickly.

### Output

```
out/the-keeper-of-skarvoy-light/
├── book.json            metadata + chapters + TTS segments (contract for later stages)
├── cover.jpg            EPUB only, when the book has a cover
├── raw/                 extracted text as Markdown (input of `refine` and `build`)
│   └── 001-part-one-chapter-1-the-fog.md
├── text/                written by `refine`: repaired, speech-ready Markdown (hand-editable)
├── refine/cache/        written by `refine`: cached model answers
└── chapters/            what the segments speak
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

## Repairing the text with an LLM (refine)

Text extracted from PDFs is rarely perfect. Even after the deterministic cleanup, words can be broken, diagram labels can be mixed into paragraphs, and tables, figures and code cannot be understood by ear. `refine` sends each chapter, in chunks, to a chat model with an editing prompt ([src/refine.rs](src/refine.rs)). The prompt asks the model to:

- repair extraction damage only: broken or joined words, stray hyphens, leftover page numbers and headers, diagram fragments. It must not summarise or reword the author's prose;
- turn tables into prose that starts with "Table X-Y, caption, …": one sentence per row for small tables, a summary for large ones;
- describe figures in a sentence from their caption and context ("Figure X-Y shows …");
- read code of 5 lines or fewer in a speakable form, and explain longer code instead of reading it;
- name hashes, keys and IDs by type only ("a SHA-256 hash"), and say only the host name of URLs;
- write symbols and abbreviations as a narrator would say them.

```sh
tts-audiobook refine out/<book> --dry-run                  # chunks, characters, estimated tokens
tts-audiobook refine out/<book> --chapters 3               # try one chapter, then read out/<book>/text/
tts-audiobook refine out/<book>                            # the rest; rebuilds book.json when done
tts-audiobook build out/<book>                             # after editing text/*.md by hand
```

The model is reached through any OpenAI-compatible `/chat/completions` API. By default this is Mistral with the `MISTRAL_API_KEY` the TTS stage already uses:

```sh
LLM_MODEL=mistral-medium-latest          # optional; or --model
LLM_BASE_URL=https://api.mistral.ai/v1   # optional; e.g. http://localhost:11434/v1 for Ollama
LLM_API_KEY=                             # optional; defaults to MISTRAL_API_KEY, not needed for local servers
```

| Option | Default | Description |
| --- | --- | --- |
| `--model <NAME>` | `LLM_MODEL`, else `mistral-medium-latest` | Chat model. |
| `--chapters <LIST>` | all | E.g. `1,3-5`. |
| `--concurrency <N>` | `4` | Parallel requests. |
| `--chunk-chars <N>` | `6000` | Characters of text per request. A table is never split. |
| `--force` | | Re-refine chapters that already have a `text/` file. This overwrites hand edits. |
| `--dry-run` | | Print what would be sent; no key needed. |
| `--max-segment-chars`, `--min-segment-chars`, `--no-ssml`, `--no-sanitize` | | As for `extract`; used when `book.json` is rebuilt. `build` takes the same options. |

- **Checked answers:** an answer is rejected and retried once, with the reason, if any of these hold:
  - it starts with commentary;
  - it drops or adds a heading;
  - it leaves a table or figure marker in;
  - it is much shorter than the input prose.

  If the second answer also fails, that chunk keeps its extracted text, and the run lists it.
- **Resumable and cached:** each accepted answer is stored in `refine/cache/`, keyed by prompt version, model and input. Re-runs, and `--force` after a small change, only pay for chunks that changed. A chapter whose request failed is not written, and the next run retries it.
- **Hand edits:** `text/*.md` is plain Markdown. Edit it, then run `build`. `refine` never overwrites an existing `text/` file without `--force`.
- **Without refine:** `book.json` is built from `raw/`. A table is then only referenced ("Table 1-2, Caption, is shown in the book."), and a figure's caption is read. `synthesize` warns when the text was not refined.

## Abridged edition (abridge)

`abridge` makes a shorter edition as a **separate book**, by default `<book>-abridged/`. The full book is not changed, and the abridged book gets its own segments and audio.

```sh
tts-audiobook abridge out/<book> --percent 50 --dry-run      # creates out/<book>-abridged, estimates cost
tts-audiobook abridge out/<book> --percent 50 --chapters 3   # try one chapter, then read out/<book>-abridged/text/
tts-audiobook abridge out/<book> --percent 50 --exclude 26-30
tts-audiobook synthesize out/<book>-abridged
tts-audiobook assemble out/<book>-abridged                   # "<Title> (Abridged).m4b"
```

1. Each chapter's best text is copied into the abridged book's `raw/`: `text/` if the chapter was refined, else `raw/`. Run `refine` on the full book first.
2. A chat model cuts each chunk to about `--percent` of its length ([src/refine.rs](src/refine.rs), `ABRIDGE_PROMPT`).
   - It removes repetition, recaps and previews, extra examples, asides, and cross-references to other chapters, pages, figures or tables.
   - It shortens long lists and reference material.
   - It keeps definitions, named concepts used later, the main arguments, conclusions and essential steps.
   - It **cuts rather than rewrites**: it keeps the author's sentences, and may add only a few connecting words.
3. Each answer is checked:
   - the chapter title is kept;
   - headings may only be dropped with their section, never added, renamed or reordered;
   - the length must land near the target, between about 40% and 160% of it, and below 95% of the input.

   A failing answer is retried once. If the retry also fails, that chunk stays unabridged, and the run lists it.
4. The abridged `book.json` is built, titled "<Title> (Abridged)", so the opening credits say it is abridged.

Options:
- `--exclude <LIST>` drops whole chapters of the full book, e.g. appendices.
- `-o <DIR>` picks another directory.
- `--chapters <LIST>` (numbers in the abridged book) limits the run.
- `--model`, `--concurrency`, `--chunk-chars`, `--force`, `--dry-run` and the `build` options work as for `refine`.

Like `refine`, the run is cached and resumable. Its `text/*.md` can be edited by hand, followed by `build out/<book>-abridged`. Running `abridge` again refreshes `raw/` from the full book, but keeps existing abridged `text/` files unless you pass `--force`.

### Markdown in raw/ and text/

`#` headings (the first is the chapter title), blank-line paragraphs, `- ` and `1. ` list items, and `* * *` section breaks. A table is `<!-- table: Table 1-1: Caption -->` followed by `| cell | cell |` rows. A figure is `<!-- figure: Figure 1-1: Caption -->`. A paragraph that starts with `\` is plain text, even if it looks like Markdown.

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
  - running headers (including `12 Part I ■ Title` style heads), footers and page numbers are removed, along with footnotes (small type at the page bottom) and superscript note markers;
  - hyphenated line breaks are rejoined, also across page and column breaks, and paragraphs are detected from gaps and indents;
  - phantom spaces that some PDFs place inside `fi`/`fl` ligatures are ignored, so words are not split;
  - a `Table N-N: caption` line starts a table: the rows (separated by the table's rules) and cells (separated by wide gaps) are collected into one table block. A `Figure N-N: caption` line becomes a figure block;
  - letter-spaced headings such as `C H A P T E R 1` are normalised to "Chapter 1: …";
  - PDF bookmarks are used as the TOC when present. If the bookmarks only list Parts, each Part is split at the "Chapter N" headings inside it.
- **Markdown / TXT:** `#` headings, lists, `* * *` scene breaks, and YAML front matter for metadata.

### Sanitising technical text

Technical documentation contains strings that are useless when read aloud. Before chapters and segments are made, every token (ignoring surrounding punctuation) of these kinds is replaced:

| Kind | Rule | Example |
| --- | --- | --- |
| Any over-long token | longer than 32 characters and not a plain word | long file paths, identifiers |
| Hex, numbers, binary | longer than 12 characters, only hex digits (optional `0x`/`0b`, `-` `:` `.` separators, a run of 8+ digits) | SHA hashes, UUIDs, `0b1101001110010110`, serial numbers |
| No words | longer than 12 characters with letters and digits jumbled together (3+ switches), or base64 padding | API tokens, `SGVsbG8gV29ybGQhIQ==` |

Each token is replaced by what it is, without its characters:

| Token | Spoken as |
| --- | --- |
| 32 / 40 / 64 / 128 hex digits | "an MD5 hash" / "a SHA-1 hash" / "a SHA-256 hash" / "a SHA-512 hash" |
| `sha256:…`, `md5=…` | the labelled type, e.g. "a SHA-256 hash" |
| UUID | "a UUID" |
| other hex / digits only / `0b…` | "a hexadecimal value" / "a long number" / "a binary number" |
| path (`/a/b/c`, `C:\…`) | "a file path" |
| base64-like / short random token | "a base64-encoded value" / "a random code" |
| anything else | "a long code" |

Punctuation is kept, so sentences still end where they did. In `NAME=value`, only the value is replaced. URLs become their host name (`https://user@docs.example.com:8443/a?b` becomes `docs.example.com`; a leading `www.` is dropped).

Readable mixes such as `Windows10Home`, `sha256sum`, `x86_64-linux-gnu`, IP addresses, MAC addresses and long compound words are left alone.

Chapter splitting chooses its split level automatically. "Part" pages are merged into the first chapter that follows ("Part One — Chapter 1: The Fog"), or become a short chapter of their own when they have an introduction. Contents, Index, Notes, Glossary, Bibliography/References, Copyright, title and EULA pages are skipped, along with short front matter.

## Development

```sh
cargo test                                   # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

The integration tests in `tests/pipeline.rs` run the same sample book in all four formats and check metadata, chapter titles, lists, scene breaks, cleanup and segment limits. They also check the `raw/` → `build` round trip, and `refine` with a fake model: caching, kept hand edits and the fallback for rejected answers. The sample book's source is `tests/fixtures/sample.md`. To regenerate the PDF/EPUB/DOCX fixtures, run `tests/fixtures/generate.ps1`, which needs pandoc and pdflatex.

## Roadmap

1. ~~Text extraction, chapter splitting, TTS preprocessing~~
2. ~~Mistral TTS synthesis of segments: one configured voice, or a voice chosen by the author's gender~~
3. ~~Audio assembly with ffmpeg: M4B/MP3, chapters, metadata, cover, AI watermark and spoken disclaimer~~
4. ~~LLM repair of extracted text (`refine`): tables, figures, code and identifiers made speakable~~
5. Open AES-256 (R6) encrypted PDFs that only have an owner password.
6. Later: an inaudible audio watermark, and a separate voice for each character.
