# dipho — Design

> Canonical design doc. The architecture summary in CLAUDE.md is derived from this; when they disagree, this wins.

## Thesis

Sentence mixing is **unit-selection speech synthesis** (the Hunt & Black 1996 lineage) where the unit database is arbitrary source media instead of a studio-recorded voice corpus. Classical unit selection picks units from a database to match a target utterance, minimizing *target cost* (does this unit sound like what we want?) plus *join cost* (do adjacent units splice cleanly?). YTP sentence mixing is exactly this problem with a found-footage database — so we build the tool around that framing instead of around a video editor's timeline.

Two core abstractions follow: **the Corpus** (the unit database) and **the Edit** (the selected, transformed unit sequence). Between them, later, sits a **solver**.

## Abstraction 1: The Corpus

An addressable phonetic index over **immutable sources**. Sources are never edited, only indexed. Everything downstream is a span reference:

```
(source_id, t_start, t_end, channel)    where channel ∈ {audio, video, both}
```

Audio and video are decoupled as first-class: a mix routinely takes the audio of one span over the video of another, holds a freeze-frame under continuing speech, or loops video under a stutter. Making `channel` part of the span reference — rather than an edit-time afterthought — keeps that representable everywhere.

### Ingest pipeline

> Revised after the 2026-06 research pass (docs/research/SUMMARY.md): WhisperX's English alignment models are letter-CTC — it emits word/character timestamps only, **never phonemes**. The phone tier needs its own alignment stage.

```
yt-dlp URL or local file
  → ffmpeg demux (normalized audio extraction: wav, mono, 16 kHz for analysis;
                  original media kept untouched for playback/render)
  → mlx-whisper (large-v3-turbo, Metal): transcription
  → WhisperX align(): word-level timestamps
  → MFA 3.x (CLI subprocess, own micromamba env, per-segment chunks):
      phone tier → diphones derived at phone midpoints
  → pyannote 4.x community-1 (MPS, run directly, HF token required):
      speaker diarization
  → librosa: prosody features per frame (f0, RMS energy)
  → write to corpus SQLite
```

MFA is the only phoneme aligner — no fallback backend (ratified 2026-06-10: best version, no degraded modes; cut-point precision is the product). The protection against MFA rot is structural, not a second pipeline: it runs as a CLI subprocess in an isolated env, so it can be swapped without touching the schema or the Rust side. Diarization is likewise a hard part of ingest: pyannote's HF token + one-time license acceptance is a documented setup prerequisite, not an optional path.

Phone labels are **ARPAbet** (MFA `english_us_arpa` models, CMUdict-compatible).

Environment strategy — learned from why sentence-mixing died (pinned MFA 1.1.0-beta binary, yt-dlp 2022): fragile tools live behind subprocess boundaries (MFA is CLI-only, in its own micromamba env; WhisperX's heavily-pinned deps isolated from everything else), and yt-dlp stays unpinned.

Ingest is batch, offline, Python (`python/` sidecar). It is never in the interactive loop. Contract: media in → one JSON document out (alignment, diarization, features), which the Rust side ingests into SQLite. The JSON contract is documented in `python/README.md`.

### Why diphones, not phonemes

Coarticulation means phoneme boundaries are the *worst* place to cut: the signal there is a transition smeared across both neighbors. The stable, cuttable points are mid-phoneme — the steady state in the middle of a vowel or fricative. A **diphone** (mid-phoneme to mid-phoneme, spanning one phoneme transition) makes its boundaries land exactly on those stable points. This is the same reason diphone synthesis predated and informed unit selection.

So the index stores diphones as the primary sub-word unit: for each adjacent phoneme pair in the aligned transcript, a span from the midpoint of phoneme A to the midpoint of phoneme B. Word- and phrase-level search sits on top for the common case ("find every time he says 'gun'"); diphone n-grams serve sub-word assembly ("build the word 'mama' out of someone who never said it").

### Storage: SQLite

One SQLite database per corpus (rusqlite, bundled — no system dependency):

- `sources` — id, origin (URL/path), media path, duration, metadata
- `words` — span ref, text, speaker, confidence; **FTS5** index for word/phrase search
- `phonemes` — span ref, phoneme label, word ref
- `diphones` — span ref (midpoint-to-midpoint), label pair (e.g. `AA-K`), plus an n-gram table over diphone label sequences for assembly queries
- `prosody` — per-unit f0 mean/contour summary, RMS energy (columns on the unit tables or a sidecar table; needed by the join-cost solver later, so it ships in the schema now)
- `speakers` — diarization clusters, optional human-assigned names

Phone rows carry a confidence column (the solver's target cost wants it); labels are ARPAbet.

Schema rule: **post-MVP features (solver, join cost) must be servable from this schema without rework.** Prosody and diphone tables exist from day one even though MVP only reads `words`.

## Abstraction 2: The Edit

A **program, not a timeline**. The edit is non-destructive EDL-as-data: an ordered structure of span references plus transforms:

```
loop, reverse, pitch, speed, stutter, ...
```

Serde-friendly format of our own (JSON or TOML — decided by what stays pleasant to hand-edit and diff). It compiles to two targets:

1. **mpv EDL** — for instant, zero-render preview. The edit recompiles to an `edl://` URI on every change and is sent to the long-lived mpv slave via `["loadfile", uri, "replace"]` — no temp files. A `.mpv.edl` file export is a user-facing artifact of the same compiler. Transforms that mpv can't express in EDL (pitch, reverse) degrade gracefully in preview or use mpv property/filter commands where possible.
2. **ffmpeg render** — final export. Full-fidelity compilation of every transform to an ffmpeg filter graph / concat. Never moviepy (videogrep's batch-of-20 + gc workarounds are a cautionary tale).

The asymmetry is deliberate: preview optimizes for latency (milliseconds, no temp files), render optimizes for correctness.

### EDL compiler contract

One choke-point Rust module (`dipho-core::edl`), both targets, golden-file tests:

- **Quoting**: every path/value quoted unconditionally with the spec's `%<byte-count>%` form (UTF-8 byte count) — naive emitters break on commas in filenames
- **Named params** (`start=`, `length=`), floats formatted explicitly (`{:.6}`), per-segment `title=` (word label; mpv's implicit chapter-per-segment gives free cut navigation)
- **Pad-then-merge semantics** (lifted from videogrep, applied before *both* targets): symmetric padding → clamp at 0 → merge overlapping/touching spans per source → clamp end to source duration
- `!new_stream` is the post-MVP mechanism for audio-from-A-over-video-from-B; `!delay_open`/`!no_clip`/`!mp4_dash` and `memory://` are out of scope
- mpv EDL v0 is explicitly unfrozen → probe mpv version at startup (also gates the `loadfile` 4th arg, added in mpv 0.38)

### mpv slave lifecycle

Spawn once: `mpv --idle=yes --keep-open=yes --no-terminal --input-ipc-server=$TMPDIR/dipho-mpv.sock` (short path — macOS `sun_path` ~104-byte limit; socket 0600 since IPC exposes `run`). One persistent JSON IPC connection for the whole session (closing drops `observe_property` registrations). Correlate replies by `request_id`, never message order; treat `playback-restart` as seek-done. Unit audition = exact seek + `ab-loop-a`/`ab-loop-b` properties (clear with `"no"`).

## The Solver (post-MVP)

Type a target sentence → ranked candidate assemblies. Classical unit-selection search:

- **Target cost** — does the candidate unit match the requested phoneme sequence (and eventually speaker, prosody)?
- **Join cost** — objective splice quality at each boundary: spectral discontinuity, f0 jump, energy jump. Explicitly **not** humor scoring; the human picks the funny one from a shortlist of clean ones.

Viterbi/beam search over the diphone lattice, same as the literature. The corpus schema (diphones + prosody) exists to feed this.

Seeds from prior art (sentence-mixing — port the ideas, not the code; see docs/research/01):

- Their 3-step cost decomposition maps 1:1 onto target cost (steps 1–2) + join cost (step 3); their weight ratios (amplitude-join ≫ spectral-join, contiguity bonus per phoneme, duration caps per phoneme class) are tuning starting points
- **Contiguity principle**: source-adjacent diphones get zero join cost plus a contiguous-span bonus — long natural runs beat technically-clean Frankenstein joins
- Candidate generation uses a phoneme substitution matrix (no hard out-of-vocabulary failures — their exact-match-only lookup hard-failed)
- Beam search over a per-position candidate lattice; join cost depends only on a short suffix (last vowel + recent RMS), which keeps the state space small
- Punctuation becomes `<BLANK>` pause targets scored by silence-RMS + duration; seeded noise diversifies alternative rankings
- videogrep-style random `mash` is the baseline the solver must demonstrably beat
- UX to keep from their CLI: chunk-by-chunk (~1 word) authoring, rank-ordered candidates, a stash buffer, re-edit, accept-and-advance, autosaved sessions — improved with a visible scored candidate list and instant mpv-EDL audition

## DSP cut refinement (post-MVP, native)

Aligner timestamps are ±tens of ms. Two refinement passes in the Rust hot loop (symphonia for decode, rustfft for analysis):

1. Snap each cut to the nearest zero-crossing within the aligner's tolerance window — eliminates clicks.
2. Later: minimize spectral flux at the boundary within that window — picks the perceptually smoothest cut point, the local version of join cost.

## Architecture: three processes

| Process | Role | Why separate |
|---|---|---|
| **dipho** (Rust) | index, search, EDL, DSP, ratatui TUI, mpv control | The interactive loop; everything latency-sensitive |
| **mpv** | slave player: preview/audition in its own window | Best-in-class playback; controlled via JSON IPC (`--input-ipc-server`); never render video in-terminal |
| **Python sidecar** | batch ingest: WhisperX, pyannote, prosody extraction | ML stack is Python-native; offline by nature; crash/version isolation |

Workspace layout:

- `crates/dipho-core` — library: span types, corpus index, EDL types + compilation, DSP. No TUI, no process management.
- `crates/dipho` — binary: clap CLI, ratatui TUI, mpv IPC client.
- `python/` — uv project, `dipho-ingest` entry point.

### TUI architecture

Elm-flavored single event loop on tokio (the convergent pattern across television, atuin, gitui, yazi — skip component frameworks): one `App` state struct, one merged `Event` enum (`Term`, `Tick`, `Mpv`, `Db`, `Job`), all producers feeding one mpsc channel, a single consumer loop, render-on-dirty with ~10 ms debounce. Module layout in `crates/dipho`: `app.rs` (state + update), `event.rs`, `ui/` (pure draw), `mpv/` (spawn + client task), `db/` (rusqlite on `spawn_blocking`).

The mpv IPC client is hand-rolled (~200 lines: `UnixStream` + request_id→oneshot map + events→mpsc) — existing crates are low-bus-factor and dipho needs precise control. Crates: now — ratatui, tokio, crossterm (event-stream), tui-input, serde_json, rusqlite; later — nucleo (fuzzy), ratatui-textarea, rodio (only if mpv audition latency disappoints); skip — mpvipc, kira.

Waveform display is built in-tree: Canvas + braille min/max envelope (Sparkline as stopgap) — no maintained terminal waveform widget exists. The per-zoom min/max/RMS peaks cache is the same data post-MVP DSP cut refinement needs, so they share it.

## MVP: one vertical slice

In order, each step usable before the next exists:

1. **Ingest** — `dipho ingest <url|file>`: run sidecar, load JSON into corpus
2. **Index** — corpus SQLite with words (FTS5), phonemes, diphones, prosody
3. **Word search** — TUI: type a word, see every utterance ranked
4. **Audition** — select a hit, mpv plays that span instantly (ab-loop for scrubbing)
5. **Flat EDL** — append spans to an edit list, reorder, save/load the EDL file, preview via compiled mpv EDL
6. **Render** — `dipho render edit.json out.mp4` via ffmpeg

Post-MVP, in rough order: transforms beyond cut/concat, diphone assembly search, the solver, DSP cut refinement, learned splice scoring. Cheap export targets once the EDL compiler exists: output-timeline VTT, FCP7 XML ("finish in a real NLE"), m3u.

## Risk register

- **mpv EDL v0 is unfrozen** — mitigated by the version probe, single serializer module, golden tests
- **MFA is conda-only in practice** (Kaldi/pynini binaries; hence the micromamba env) and arm64-on-M4 is not yet smoke-tested — this is the first task of M2, so failure surfaces in an hour, not mid-milestone. An MFA version pin is exactly what killed sentence-mixing; the subprocess boundary (swappable without schema changes) is the mitigation.
- **mpv audition latency on sub-second units is unmeasured** — measure before adopting any audio crate; rodio + pre-decoded PCM is the fallback
- **pyannote community-1 is HF-gated** — ingest needs an HF token + one-time license acceptance (hard setup prerequisite; diarization is not optional)
- **Dependency rot killed the prior art** — counter-policy: yt-dlp unpinned, fragile tools behind subprocess boundaries
- mlx-whisper has a reported memory-growth issue with `word_timestamps` on long audio — don't enable it; word times come from WhisperX align

## Open questions

Resolved by the 2026-06 research pass: WhisperX is word-level only (MFA supplies phones); Whisper backend on Apple Silicon is mlx-whisper (faster-whisper has no Metal backend).

Ratified 2026-06-10: MFA is the sole phoneme aligner, no fallback backend (best version over degraded modes); ARPAbet phone set (`english_us_arpa`); HF token is a hard ingest prerequisite; mpv floor is ≥0.38 enforced by the startup probe (no bundling).

Still open:

- Latency budget for audition: what's "good enough" via mpv before investing in a rodio PCM path? (measure in M4)
- EDL file format: JSON vs TOML (lean JSON for nested structure; revisit when the EDL type stabilizes)
