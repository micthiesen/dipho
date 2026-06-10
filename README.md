# dipho

A TUI for making YTPs and sentence mixes.

**The idea:** sentence mixing is unit-selection speech synthesis where the unit database is whatever media you point it at. dipho is built on two abstractions:

1. **The Corpus** — an addressable phonetic index over immutable sources. Ingest a video and every word, phoneme, and diphone in it becomes searchable, with speaker labels and prosody features. Sources are never edited — everything is a span reference, with audio and video as independently addressable channels.
2. **The Edit** — a program, not a timeline. A non-destructive edit list of span references plus transforms (loop, reverse, pitch, stutter) that compiles to an mpv EDL for instant zero-render preview, and to ffmpeg for final export.

Later, a solver sits between them: type a sentence, get ranked candidate assemblies scored by splice quality.

See [DESIGN.md](DESIGN.md) for the full design.

## Status

Pre-MVP scaffold. Nothing works yet.

## Stack

- Rust (ratatui TUI, rusqlite corpus, mpv via JSON IPC)
- mpv as the external preview player
- Python sidecar (`python/`) for batch ingest: WhisperX alignment + pyannote diarization
