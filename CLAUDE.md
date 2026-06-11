# dipho

A TUI tool for making YTPs and sentence mixes. Core thesis: sentence mixing is unit-selection speech synthesis where the unit database is arbitrary source media.

> This is a living document. Update it when you learn new preferences, patterns, or project conventions. Don't ask—just update it if something is missing or outdated.

@DESIGN.md

## Quick Reference

```bash
cargo build                     # Build the workspace
cargo test                      # Run all tests
cargo clippy -- -D warnings     # Lint (warnings are errors)
cargo fmt                       # Format code
cargo run -p dipho              # Run the TUI (search + audition + EDL editing/preview;
                                #   --edit overrides ./edit.json)
cargo run -p dipho -- ingest <url|file>   # Build the corpus (--corpus overrides ./.dipho/corpus.db)
cargo run -p dipho -- search "twenty five"  # Word/phrase search, exact word spans
cargo run -p dipho -- render edit.json out.mp4  # Two-stage ffmpeg render (--size/--fps override)

# Python ingest sidecar (batch only, never in the interactive loop)
cd python && uv sync            # Install sidecar deps
cd python && uv run dipho-ingest --help
```

**Always run `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` after making changes.**

## Architecture

Three processes, each doing what it's best at:

- **Rust core** (this workspace): corpus index, search, EDL, DSP, ratatui TUI, mpv control via JSON IPC
- **mpv**: external window slave player for preview/audition (`--input-ipc-server`). Never render video in-terminal.
- **Python sidecar** (`python/`): batch ingest only (mlx-whisper transcribe → WhisperX word align → MFA phone align → pyannote diarization → librosa prosody). Contract: staged work dir + `manifest.json`, NDJSON progress on stdout (see python/README.md).

```
crates/
├── dipho-core/          # Library: no TUI, no I/O policy
│   └── src/
│       ├── span.rs      # Span references: (source_id, t_start, t_end, channel)
│       ├── corpus/      # SQLite index (rusqlite bundled): schema, loader, FTS5
│       │                #   search + query normalization (num2words port)
│       ├── edl/         # EDL-as-data: types, validation + shared elision pre-pass
│       │                #   (plan_preview), mpv EDL + ffmpeg render compilers,
│       │                #   rebind precedence
│       └── dsp.rs       # Cut refinement (zero-crossing snap, spectral flux) — stub
├── dipho/               # Binary: clap CLI + ratatui TUI shell
│   └── src/
│       ├── main.rs      # CLI entry (clap subcommands, global --corpus)
│       ├── ingest/      # Staged ingest driver: origin/idempotency, master+wav, sidecar
│       ├── search.rs    # `dipho search` CLI
│       ├── render.rs    # `dipho render` CLI: profile resolution, plan execution
│       ├── tui/         # Elm-style event loop: app, event, db (reader thread),
│       │                #   edit (EDL session: undo/autosave/save), player
│       │                #   (mpv audition + preview actor), ui
│       └── mpv.rs       # mpv JSON IPC client + slave lifecycle (spawn, probe)
python/                  # uv project: ingest sidecar (WhisperX + pyannote planned)
docs/                    # DESIGN.md is canonical; research reports in docs/research/
```

Two core abstractions (see DESIGN.md for the full treatment):

1. **The Corpus** — an addressable phonetic index over immutable sources. Sources are never edited, only indexed. Everything is a span reference; audio and video channels are decoupled. Diphones, not phonemes, are the indexed unit.
2. **The Edit** — a program, not a timeline. Non-destructive EDL-as-data that compiles to mpv EDL (instant preview) and ffmpeg (final render).

## Code Style

- `cargo fmt` for formatting (default config), `clippy -D warnings` must pass
- **Strong types**: newtypes over bare primitives, enums and exhaustive matches over flags
- **No over-engineering**: simple solutions, no unnecessary abstractions; add generality when a second concrete use case demands it, not before
- **Clean code**: no debug leftovers, no commented-out code, no `println!` debugging in committed code
- Errors: `thiserror` in `dipho-core`, `anyhow` in the binary

## Testing

Test pure logic (span math, EDL compilation, schema queries against in-memory SQLite); don't test mpv/ffmpeg/network. Unit tests live next to the code in `#[cfg(test)]` modules.

## Commits

One-line imperative summaries, sentence case, no type prefixes: "Add diphone index table", not "feat: added diphone index table".

This is a personal product: commit and push directly to main. The global PR workflow (draft PRs, size budgets, stacking, `/split`) does not apply here.

## Platform Notes (Apple Silicon, M4 Max)

- arm64; Homebrew at `/opt/homebrew`
- Rust toolchain via rustup (`~/.cargo/bin`), pinned stable in `rust-toolchain.toml`
- mpv, ffmpeg, yt-dlp via brew
- Whisper inference runs locally via mlx-whisper (Metal) — faster-whisper/CTranslate2 has no Metal backend; pyannote 4.x runs on MPS (see docs/research/04-alignment-stack.md)
- Python sidecar uses `uv` exclusively (no pip, no poetry)

## MVP Scope

Vertical slice, in order: ingest → index → word search → audition via mpv → flat EDL → ffmpeg render. The solver, join-cost ranking, and learned splice scoring are post-MVP — but the schema must support them without rework.
